import importlib.util
import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

# release-prep.py uses a hyphen — import it by path.
_RELEASE_PREP_PATH = pathlib.Path(__file__).resolve().parents[2] / "release-prep.py"
_spec = importlib.util.spec_from_file_location("release_prep", _RELEASE_PREP_PATH)
release_prep = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(release_prep)

_FIXTURE_ROOT = pathlib.Path(__file__).parent / "fixtures" / "release_prep"


def _fragment_dict(
    kind: str = "capability",
    surfaces: list[str] | None = None,
    readme_bullet: str | None = None,
    body: str = "",
    issues: list[int] | None = None,
    prs: list[int] | None = None,
) -> tuple[dict, str]:
    fm: dict = {"kind": kind}
    if surfaces is not None:
        fm["surfaces"] = surfaces
    if readme_bullet is not None:
        fm["readme_bullet"] = readme_bullet
    if issues is not None:
        fm["issues"] = issues
    if prs is not None:
        fm["prs"] = prs
    return fm, body


def _fixture_fragments(name: str) -> list[tuple[pathlib.Path, dict, str]]:
    fragments = []
    for path in sorted((_FIXTURE_ROOT / name).glob("*.md")):
        fm, body = release_prep.parse_front_matter(path.read_text(encoding="utf-8"))
        fragments.append((path, fm, body))
    return fragments


def _fixture_text(name: str) -> str:
    return (_FIXTURE_ROOT / name).read_text(encoding="utf-8")


def _run_release_prep(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(_RELEASE_PREP_PATH), *args],
        capture_output=True,
        text=True,
        check=False,
    )


class CompileEntryTests(unittest.TestCase):
    def test_breaking_fragment_produces_breaking_section(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict("breaking", [], body="Delete old key.")),
        ]
        entry = release_prep._compile_entry(fragments)
        self.assertIn("### Breaking", entry)
        self.assertIn("Delete old key.", entry)

    def test_capability_produces_highlights_and_added(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "capability", ["readme"], readme_bullet="**X**",
                body="User can now: do X\n\nDetail: mechanism\n",
            )),
        ]
        entry = release_prep._compile_entry(fragments)
        self.assertIn("### Highlights", entry)
        self.assertIn("**X**. Do X.", entry)
        self.assertIn("### Added", entry)
        self.assertIn("mechanism", entry)

    def test_capability_highlight_includes_name_prose_and_chapter(self):
        fm, body = _fragment_dict(
            "capability", ["readme"],
            readme_bullet="**MQTT state publishing** — opt-in retained state",
            body="User can now: mirror live state into Home Assistant.\n",
        )
        fm["chapter"] = "mqtt-publishing.md"

        entry = release_prep._compile_entry([(pathlib.Path("a.md"), fm, body)])

        self.assertIn(
            "**MQTT state publishing** — opt-in retained state. Mirror live state "
            "into Home Assistant. See [the MQTT state publishing chapter]"
            "(./docs/src/mqtt-publishing.md).",
            entry,
        )

    def test_capability_highlight_without_chapter_has_no_dangling_link(self):
        fm, body = _fragment_dict(
            "capability", ["readme"],
            readme_bullet="**Config rail** — jump to any section",
            body="User can now: jump to any section.\n",
        )

        entry = release_prep._compile_entry([(pathlib.Path("a.md"), fm, body)])

        self.assertIn(
            "**Config rail** — jump to any section.", entry,
        )
        self.assertNotIn("Jump to any section. Jump to any section.", entry)
        self.assertNotIn("See [", entry)

    def test_redundant_highlight_prose_is_emitted_once(self):
        fm, body = _fragment_dict(
            "capability", ["readme"],
            readme_bullet="Users can export reports",
            body="User can now: users can export reports.\n",
        )

        entry = release_prep._compile_entry([(pathlib.Path("a.md"), fm, body)])

        self.assertEqual(entry.count("Users can export reports."), 1)

    def test_different_highlight_prose_is_kept(self):
        fm, body = _fragment_dict(
            "capability", ["readme"],
            readme_bullet="**Reports** — export reports",
            body="User can now: monitor report delivery.\n",
        )

        entry = release_prep._compile_entry([(pathlib.Path("a.md"), fm, body)])

        self.assertIn("**Reports** — export reports. Monitor report delivery.", entry)

    def test_capability_highlights_are_separate_paragraphs_in_sort_order(self):
        first_fm, first_body = _fragment_dict(
            "capability", ["readme"],
            readme_bullet="**First** — first capability",
            body="User can now: use the first capability.\n",
        )
        second_fm, second_body = _fragment_dict(
            "capability", ["readme"],
            readme_bullet="**Second** — second capability",
            body="User can now: use the second capability.\n",
        )

        entry = release_prep._compile_entry([
            (pathlib.Path("a.md"), first_fm, first_body),
            (pathlib.Path("b.md"), second_fm, second_body),
        ])

        first = "**First** — first capability. Use the first capability."
        second = "**Second** — second capability. Use the second capability."
        self.assertLess(entry.index(first), entry.index(second))
        self.assertIn(f"{first}\n\n{second}", entry)

    def test_improvement_produces_changed(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "improvement", [], body="Detail: better thing\n",
            )),
        ]
        entry = release_prep._compile_entry(fragments)
        self.assertIn("### Changed", entry)
        self.assertIn("better thing", entry)

    def test_fix_produces_fixed(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "fix", [], body="Detail: fixed crash\n",
            )),
        ]
        entry = release_prep._compile_entry(fragments)
        self.assertIn("### Fixed", entry)
        self.assertIn("fixed crash", entry)

    def test_breaking_with_detail_produces_both_breaking_and_removed(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "breaking", [], body="Delete old key.\n\nDetail: removed old config\n",
            )),
        ]
        entry = release_prep._compile_entry(fragments)
        self.assertIn("### Breaking", entry)
        self.assertIn("Delete old key.", entry)
        self.assertIn("### Removed", entry)
        self.assertIn("removed old config", entry)

    def test_no_capability_no_highlights(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict("improvement", [], body="Detail: tweak\n")),
        ]
        entry = release_prep._compile_entry(fragments)
        self.assertNotIn("### Highlights", entry)

    def test_section_order_is_breaking_highlights_added_changed_fixed_removed(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict("breaking", [], body="Migration.")),
            (pathlib.Path("b.md"), *_fragment_dict("capability", ["readme"], "**X**", "User can now: do\n\nDetail: added\n")),
            (pathlib.Path("c.md"), *_fragment_dict("improvement", [], body="Detail: changed\n")),
            (pathlib.Path("d.md"), *_fragment_dict("fix", [], body="Detail: fixed\n")),
        ]
        entry = release_prep._compile_entry(fragments)
        lines = entry.splitlines()
        breaking_idx = next(i for i, l in enumerate(lines) if "### Breaking" in l)
        highlights_idx = next(i for i, l in enumerate(lines) if "### Highlights" in l)
        added_idx = next(i for i, l in enumerate(lines) if "### Added" in l)
        changed_idx = next(i for i, l in enumerate(lines) if "### Changed" in l)
        fixed_idx = next(i for i, l in enumerate(lines) if "### Fixed" in l)
        self.assertLess(breaking_idx, highlights_idx)
        self.assertLess(highlights_idx, added_idx)
        self.assertLess(added_idx, changed_idx)
        self.assertLess(changed_idx, fixed_idx)


class SurfaceValidationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def _write_readme(self, content: str) -> None:
        (self.root / "README.md").write_text(content, encoding="utf-8")

    def test_readme_bullet_present_passes(self):
        self._write_readme("# dormant\n\n## What it does\n- **X** — does Y\n")
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "capability", ["readme"], readme_bullet="**X** — does Y",
                body="User can now: do X\n",
            )),
        ]
        errors = release_prep._validate_surfaces(self.root, fragments)
        self.assertEqual(errors, [])

    def test_readme_bullet_absent_fails(self):
        self._write_readme("# dormant\n\n## What it does\n- **Y** — does Z\n")
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "capability", ["readme"], readme_bullet="**X** — does Y",
                body="User can now: do X\n",
            )),
        ]
        errors = release_prep._validate_surfaces(self.root, fragments)
        self.assertTrue(any("absent from README.md" in e for e in errors), errors)

    def test_surfaces_readme_without_bullet_fails(self):
        self._write_readme("# dormant\n")
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "capability", ["readme"], readme_bullet="",
                body="User can now: do X\n",
            )),
        ]
        errors = release_prep._validate_surfaces(self.root, fragments)
        self.assertTrue(any("readme_bullet is missing" in e for e in errors), errors)

    def test_no_readme_in_surfaces_skips_check(self):
        # Surfaces without 'readme' skip the README check; empty surfaces
        # skip both checks.
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "capability", [], body="User can now: do X\n",
            )),
        ]
        errors = release_prep._validate_surfaces(self.root, fragments)
        self.assertEqual(errors, [])


class HighlightsCheckTests(unittest.TestCase):
    def test_has_highlights_when_present(self):
        entry = "### Highlights\n**do X**\n\n### Added\n"
        self.assertTrue(release_prep._highlights_present(entry))

    def test_missing_highlights_when_absent(self):
        entry = "### Added\n- thing\n"
        self.assertFalse(release_prep._highlights_present(entry))

    def test_has_capability_detects_capability_fragment(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict("capability", ["readme"], body="User can now: do\n")),
            (pathlib.Path("b.md"), *_fragment_dict("improvement", [])),
        ]
        self.assertTrue(release_prep._has_capability(fragments))

    def test_has_capability_false_without_capability(self):
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict("improvement", [])),
            (pathlib.Path("b.md"), *_fragment_dict("fix", [])),
        ]
        self.assertFalse(release_prep._has_capability(fragments))


class ExtractTests(unittest.TestCase):
    def test_extract_user_can_now(self):
        body = "User can now: do something\n\nDetail: mechanism\n"
        self.assertEqual(release_prep._extract_user_can_now(body), "do something")

    def test_extract_user_can_now_reads_multiline_paragraph(self):
        # `User can now:` is paragraph-aware (issue #156): continuation lines
        # joined with spaces until the next blank line. The previous
        # implementation only read same-line text and silently truncated
        # every paragraph that wrapped onto a second physical line.
        body = "User can now: do X across\nmultiple machines\n\nDetail: mechanism\n"
        self.assertEqual(
            release_prep._extract_user_can_now(body),
            "do X across multiple machines",
        )

    def test_detail_less_fix_uses_fragment_body(self):
        # A `fix` fragment without a `Detail:` marker must still produce a
        # Fixed bullet — its plain body becomes the bullet text (issue #156).
        fragments = [
            (pathlib.Path("a.md"), *_fragment_dict(
                "fix", [], body="fixed a regression where X\n",
            )),
        ]
        entry = release_prep._compile_entry(fragments)
        self.assertIn("### Fixed", entry)
        self.assertIn("fixed a regression where X", entry)

    def test_extract_detail(self):
        body = "User can now: do X\n\nDetail: the mechanism works\n"
        self.assertEqual(release_prep._extract_detail(body), "the mechanism works")

    def test_extract_detail_multiline(self):
        body = "User can now: do X\n\nDetail: first line.\nSecond line.\n"
        self.assertEqual(release_prep._extract_detail(body), "first line. Second line.")

    def test_extract_detail_not_present(self):
        body = "User can now: do X\n"
        self.assertEqual(release_prep._extract_detail(body), "")


class CoverageGateTests(unittest.TestCase):
    def test_normalizer_preserves_period_after_linked_citations(self):
        # CHANGELOG.md uses `text (#refs).` 26 times and `text. (#refs)` zero times.
        cited_before_period = (
            "MQTT hook actions now publish through the configured sensor-plane broker "
            "and credentials, including after configuration reloads "
            "([#230](https://example.test/230), [#238](https://example.test/238))."
        )
        cited_after_period = (
            "MQTT hook actions now publish through the configured sensor-plane broker "
            "and credentials, including after configuration reloads. "
            "([#230](https://example.test/230), [#238](https://example.test/238))"
        )
        uncited = (
            "MQTT hook actions now publish through the configured sensor-plane broker "
            "and credentials, including after configuration reloads."
        )

        self.assertEqual(
            release_prep._normalize_coverage_text(cited_before_period),
            release_prep._normalize_coverage_text(uncited),
        )
        self.assertEqual(
            release_prep._normalize_coverage_text(cited_after_period),
            release_prep._normalize_coverage_text(uncited),
        )

    def test_v012_truncated_section_reports_every_missing_emitted_entry(self):
        fragments = _fixture_fragments("v0_12_0")
        entry = release_prep._compile_entry(fragments)

        errors = release_prep._coverage_errors(
            entry, _fixture_text("v0_12_0_truncated_section.md"),
        )

        self.assertEqual(len(errors), 8)
        self.assertTrue(any("Add an injectable MQTT event-loop seam" in e for e in errors))
        self.assertTrue(any("Fix a false alarm in the wear heat map" in e for e in errors))

    def test_v012_reordered_linked_section_passes_normalized_coverage(self):
        fragments = _fixture_fragments("v0_12_0")
        entry = release_prep._compile_entry(fragments)

        errors = release_prep._coverage_errors(
            entry, _fixture_text("v0_12_0_linked_reordered_section.md"),
        )

        self.assertEqual(errors, [])

    def test_hand_curated_v012_repair_was_still_lossy(self):
        fragments = _fixture_fragments("v0_12_0")
        entry = release_prep._compile_entry(fragments)

        # The historical repair dropped entries and reworded prose; strict coverage rejects both.
        errors = release_prep._coverage_errors(
            entry, _fixture_text("v0_12_0_curated_section.md"),
        )

        self.assertTrue(any("Add an injectable MQTT event-loop seam" in e for e in errors))
        self.assertTrue(any("NoDisplay=true" in e for e in errors))

    def test_extracts_newest_version_section(self):
        body = "intro\n\n## [1.2.3] - 2026-08-06\nnew\n\n## [1.2.2] - 2026-08-05\nold\n"

        self.assertEqual(
            release_prep._extract_newest_version_section(body),
            "## [1.2.3] - 2026-08-06\nnew\n\n",
        )

    def test_delete_refuses_and_preserves_fragments_when_coverage_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fragment_dir = root / "fragments"
            fragment_dir.mkdir()
            fragment = fragment_dir / "fix.md"
            fragment.write_text(
                "---\nkind: fix\nsurfaces: []\n---\nDetail: must remain\n",
                encoding="utf-8",
            )
            changelog = root / "CHANGELOG.md"
            changelog.write_text("## [0.12.0] - 2026-08-06\n\n", encoding="utf-8")

            result = _run_release_prep(
                "--fragment-dir", str(fragment_dir),
                "--changelog", str(changelog),
                "--delete-fragments",
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(fragment.exists())
            self.assertIn("coverage", result.stderr.lower())

    def test_delete_succeeds_and_removes_fragments_when_coverage_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fragment_dir = root / "fragments"
            fragment_dir.mkdir()
            fragment = fragment_dir / "fix.md"
            fragment.write_text(
                "---\nkind: fix\nsurfaces: []\n---\nDetail: fixed thing\n",
                encoding="utf-8",
            )
            changelog = root / "CHANGELOG.md"
            changelog.write_text(
                "## [0.12.0] - 2026-08-06\n\n### Fixed\n\n- fixed thing ([#1](https://example.test/issues/1))\n",
                encoding="utf-8",
            )

            result = _run_release_prep(
                "--fragment-dir", str(fragment_dir),
                "--changelog", str(changelog),
                "--delete-fragments",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(fragment.exists())

    def test_bare_check_ignores_coverage_so_pre_release_check_stays_green(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fragment_dir = root / "fragments"
            fragment_dir.mkdir()
            (fragment_dir / "fix.md").write_text(
                "---\nkind: fix\nsurfaces: []\n---\nDetail: not released yet\n",
                encoding="utf-8",
            )

            result = _run_release_prep("--fragment-dir", str(fragment_dir), "--check")

            self.assertEqual(result.returncode, 0, result.stderr)


class CitationTests(unittest.TestCase):
    def test_issues_only_citation(self):
        fm, body = _fragment_dict("fix", [], body="Detail: fixed thing\n", issues=[234, 216])
        entry = release_prep._compile_entry([(pathlib.Path("a.md"), fm, body)])
        self.assertIn(
            "fixed thing. ([#234](https://github.com/legion-works/dormant/issues/234), "
            "[#216](https://github.com/legion-works/dormant/issues/216))",
            entry,
        )

    def test_prs_only_citation(self):
        fm, body = _fragment_dict("fix", [], body="Detail: fixed thing\n", prs=[237])
        entry = release_prep._compile_entry([(pathlib.Path("a.md"), fm, body)])
        self.assertIn(
            "fixed thing. ([#237](https://github.com/legion-works/dormant/pull/237))",
            entry,
        )

    def test_both_citations_put_issues_before_prs(self):
        fm, body = _fragment_dict(
            "fix", [], body="Detail: fixed thing\n", issues=[234], prs=[237],
        )
        entry = release_prep._compile_entry([(pathlib.Path("a.md"), fm, body)])
        self.assertIn(
            "fixed thing. ([#234](https://github.com/legion-works/dormant/issues/234), "
            "[#237](https://github.com/legion-works/dormant/pull/237))",
            entry,
        )

    def test_absent_citations_preserve_previous_output(self):
        old_fm, old_body = _fragment_dict("fix", [], body="Detail: fixed thing\n")
        new_fm, new_body = _fragment_dict("fix", [], body="Detail: fixed thing\n", issues=[])
        old_entry = release_prep._compile_entry([(pathlib.Path("a.md"), old_fm, old_body)])
        new_entry = release_prep._compile_entry([(pathlib.Path("a.md"), new_fm, new_body)])
        self.assertEqual(old_entry, new_entry)


class WriteModeTests(unittest.TestCase):
    def test_write_preserves_exact_changelog_spacing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fragment_dir = root / "fragments"
            fragment_dir.mkdir()
            (fragment_dir / "fix.md").write_text(
                "---\nkind: fix\nsurfaces: []\n---\nDetail: fixed thing\n",
                encoding="utf-8",
            )
            changelog = root / "CHANGELOG.md"
            changelog.write_text(
                _fixture_text("changelog_format.md"), encoding="utf-8",
            )

            result = _run_release_prep(
                "--fragment-dir", str(fragment_dir),
                "--changelog", str(changelog),
                "--write",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                changelog.read_text(encoding="utf-8"),
                "# Changelog\n\n"
                "All notable changes to `dormant` are recorded here.\n\n"
                "The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), "
                "and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html).\n\n"
                "## [0.12.1] - 2026-08-06\n\n"
                "### Fixed\n\n"
                "- fixed thing\n\n"
                "## [0.12.0] - 2026-08-06\n\n"
                "### Fixed\n\n"
                "- old\n",
            )

    def test_write_inserts_versioned_entry_at_changelog_anchor(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fragment_dir = root / "fragments"
            fragment_dir.mkdir()
            (fragment_dir / "fix.md").write_text(
                "---\nkind: fix\nsurfaces: []\n---\nDetail: fixed thing\n",
                encoding="utf-8",
            )
            changelog = root / "CHANGELOG.md"
            changelog.write_text(
                "# Changelog\n\n"
                "The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), "
                "and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html).\n\n"
                "## [0.12.0] - 2026-08-06\n\n### Fixed\n- old\n",
                encoding="utf-8",
            )

            result = _run_release_prep(
                "--fragment-dir", str(fragment_dir),
                "--changelog", str(changelog),
                "--write",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            content = changelog.read_text(encoding="utf-8")
            self.assertIn("## [0.12.1] - 2026-08-06", content)
            self.assertLess(content.index("## [0.12.1]"), content.index("## [0.12.0]"))
            self.assertIn("- fixed thing", content)

    def test_write_output_with_citations_round_trips_coverage(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fragment_dir = root / "fragments"
            fragment_dir.mkdir()
            fragment = fragment_dir / "fix.md"
            fragment.write_text(
                "---\nkind: fix\nsurfaces: []\nissues: [1]\n---\nDetail: fixed thing\n",
                encoding="utf-8",
            )
            changelog = root / "CHANGELOG.md"
            changelog.write_text(
                "# Changelog\n\n"
                "The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), "
                "and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html).\n\n"
                "## [0.12.0] - 2026-08-06\n\n### Fixed\n- old\n",
                encoding="utf-8",
            )

            result = _run_release_prep(
                "--fragment-dir", str(fragment_dir), "--changelog", str(changelog), "--write",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            fm, body = release_prep.parse_front_matter(fragment.read_text(encoding="utf-8"))
            entry = release_prep._compile_entry([(fragment, fm, body)])
            section = release_prep._extract_newest_version_section(
                changelog.read_text(encoding="utf-8"),
            )
            self.assertEqual(release_prep._coverage_errors(entry, section), [])

    def test_write_refuses_duplicate_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fragment_dir = root / "fragments"
            fragment_dir.mkdir()
            (fragment_dir / "fix.md").write_text(
                "---\nkind: fix\nsurfaces: []\n---\nDetail: fixed thing\n",
                encoding="utf-8",
            )
            changelog = root / "CHANGELOG.md"
            changelog.write_text("## [0.12.1] - 2026-08-06\n\n", encoding="utf-8")

            result = _run_release_prep(
                "--fragment-dir", str(fragment_dir),
                "--changelog", str(changelog),
                "--write",
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("already exists", result.stderr)
            self.assertTrue((fragment_dir / "fix.md").exists())


class WorkspaceVersionTests(unittest.TestCase):
    def test_missing_workspace_package_section_has_actionable_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "workspace package section is missing"):
                release_prep._workspace_version(root)


if __name__ == "__main__":
    unittest.main()

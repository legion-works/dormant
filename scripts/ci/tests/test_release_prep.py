import importlib.util
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

# release-prep.py uses a hyphen — import it by path.
_RELEASE_PREP_PATH = pathlib.Path(__file__).resolve().parents[2] / "release-prep.py"
_spec = importlib.util.spec_from_file_location("release_prep", _RELEASE_PREP_PATH)
release_prep = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(release_prep)


def _fragment_dict(
    kind: str = "capability",
    surfaces: list[str] | None = None,
    readme_bullet: str | None = None,
    body: str = "",
) -> tuple[dict, str]:
    fm: dict = {"kind": kind}
    if surfaces is not None:
        fm["surfaces"] = surfaces
    if readme_bullet is not None:
        fm["readme_bullet"] = readme_bullet
    return fm, body


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
            "**Config rail** — jump to any section. Jump to any section.", entry,
        )
        self.assertNotIn("See [", entry)

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


if __name__ == "__main__":
    unittest.main()

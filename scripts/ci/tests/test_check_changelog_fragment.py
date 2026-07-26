import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import check_changelog_fragment


def _write_fragment(dir_path: pathlib.Path, name: str, content: str) -> pathlib.Path:
    path = dir_path / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return path


class FrontMatterParsingTests(unittest.TestCase):
    def test_parses_kind_and_surfaces(self):
        fm, body = check_changelog_fragment.parse_front_matter(
            "---\nkind: capability\nsurfaces: [readme, chapter]\n---\nUser can now: do something\n"
        )
        self.assertEqual(fm["kind"], "capability")
        self.assertEqual(fm["surfaces"], ["readme", "chapter"])
        self.assertIn("do something", body)

    def test_parses_quoted_readme_bullet(self):
        fm, _body = check_changelog_fragment.parse_front_matter(
            '---\nkind: capability\nsurfaces: [readme]\nreadme_bullet: "**Bullet** — text"\n---\nUser can now: do\n'
        )
        self.assertEqual(fm["readme_bullet"], "**Bullet** — text")

    def test_parses_empty_surfaces(self):
        fm, _body = check_changelog_fragment.parse_front_matter(
            "---\nkind: improvement\nsurfaces: []\n---\nDetail: fixed\n"
        )
        self.assertEqual(fm["surfaces"], [])

    def test_parses_single_quoted_item_in_list(self):
        fm, _body = check_changelog_fragment.parse_front_matter(
            "---\nkind: capability\nsurfaces: ['readme']\n---\nUser can now: do\n"
        )
        self.assertEqual(fm["surfaces"], ["readme"])

    def test_rejects_missing_opening_delimiter(self):
        with self.assertRaises(ValueError):
            check_changelog_fragment.parse_front_matter("kind: fix\n---\nbody\n")

    def test_rejects_missing_closing_delimiter(self):
        with self.assertRaises(ValueError):
            check_changelog_fragment.parse_front_matter("---\nkind: fix\nbody\n")


class FragmentValidationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def _frag(self, content: str, name: str = "feat-x.md") -> str:
        _write_fragment(self.root, name, content)
        return name

    def _validate(self, name: str) -> list[str]:
        return check_changelog_fragment.validate_fragment(self.root, name)

    def test_valid_capability_passes(self):
        name = self._frag(
            "---\nkind: capability\nsurfaces: [readme]\nreadme_bullet: \"**X** — y\"\n---\nUser can now: do X\n\nDetail: mechanism\n"
        )
        self.assertEqual(self._validate(name), [])

    def test_valid_improvement_empty_surfaces_passes(self):
        name = self._frag(
            "---\nkind: improvement\nsurfaces: []\n---\nDetail: better thing\n"
        )
        self.assertEqual(self._validate(name), [])

    def test_valid_fix_passes(self):
        name = self._frag(
            "---\nkind: fix\nsurfaces: []\n---\nDetail: fixed crash\n"
        )
        self.assertEqual(self._validate(name), [])

    def test_valid_breaking_passes(self):
        name = self._frag(
            "---\nkind: breaking\nsurfaces: []\n---\nDelete the old config key and run dormantctl validate before upgrading.\n"
        )
        self.assertEqual(self._validate(name), [])

    def test_capability_without_user_can_now_fails(self):
        name = self._frag(
            "---\nkind: capability\nsurfaces: [readme]\nreadme_bullet: \"**X**\"\n---\nDetail: but no user can now\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("User can now" in e for e in errors), errors)

    def test_capability_with_empty_surfaces_fails(self):
        name = self._frag(
            "---\nkind: capability\nsurfaces: []\n---\nUser can now: do X\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("at least one surface" in e for e in errors), errors)

    def test_surfaces_readme_without_bullet_fails(self):
        name = self._frag(
            "---\nkind: capability\nsurfaces: [readme]\n---\nUser can now: do X\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("readme_bullet" in e for e in errors), errors)

    def test_surfaces_readme_with_empty_bullet_fails(self):
        name = self._frag(
            "---\nkind: capability\nsurfaces: [readme]\nreadme_bullet: \"\"\n---\nUser can now: do X\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("readme_bullet" in e for e in errors), errors)

    def test_breaking_without_body_fails(self):
        name = self._frag(
            "---\nkind: breaking\nsurfaces: []\n---\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("migration sentence" in e for e in errors), errors)

    def test_invalid_kind_fails(self):
        name = self._frag(
            "---\nkind: feature\nsurfaces: []\n---\nbody\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("kind must be one of" in e for e in errors), errors)

    def test_missing_kind_fails(self):
        name = self._frag(
            "---\nsurfaces: []\n---\nbody\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("kind is required" in e for e in errors), errors)

    def test_surfaces_not_a_list_fails(self):
        name = self._frag(
            "---\nkind: fix\nsurfaces: readme\n---\nbody\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("surfaces must be a list" in e for e in errors), errors)

    def test_user_can_now_case_insensitive(self):
        name = self._frag(
            "---\nkind: capability\nsurfaces: [readme]\nreadme_bullet: \"**X**\"\n---\nUSER CAN NOW: do X\n"
        )
        self.assertEqual(self._validate(name), [])

    def test_user_can_now_needs_content(self):
        name = self._frag(
            "---\nkind: capability\nsurfaces: [readme]\nreadme_bullet: \"**X**\"\n---\nUser can now:\n"
        )
        errors = self._validate(name)
        self.assertTrue(any("User can now" in e for e in errors), errors)

    # -- Error messages use repo-relative paths --
    def test_error_message_uses_relative_path(self):
        name = self._frag(
            "---\nkind: feature\nsurfaces: []\n---\nbody\n"
        )
        errors = self._validate(name)
        self.assertTrue(
            any(name in e and not str(self.root) in e for e in errors),
            f"expected relative path in errors, got: {errors}",
        )

    # -- Absent-at-HEAD fragments (consumed by a release) --
    def test_absent_fragment_skips_silently(self):
        # A fragment that existed in the range but was deleted before HEAD
        # (e.g. consumed by a release commit) must pass without errors.
        name = "feat-released.md"
        # Never write the file — simulate a deleted fragment.
        errors = check_changelog_fragment.validate_fragment(self.root, name)
        self.assertEqual(errors, [])


class TitleDetectionTests(unittest.TestCase):
    def test_feat_colon_matches(self):
        self.assertTrue(check_changelog_fragment._is_title_feat_or_fix("feat: add widget"))

    def test_feat_with_scope_matches(self):
        self.assertTrue(check_changelog_fragment._is_title_feat_or_fix("feat(sensors): add LD2412 parser"))

    def test_fix_colon_matches(self):
        self.assertTrue(check_changelog_fragment._is_title_feat_or_fix("fix: crash on empty config"))

    def test_fix_with_scope_matches(self):
        self.assertTrue(check_changelog_fragment._is_title_feat_or_fix("fix(displays): handle timeout"))

    def test_chore_does_not_match(self):
        self.assertFalse(check_changelog_fragment._is_title_feat_or_fix("chore: update deps"))

    def test_docs_does_not_match(self):
        self.assertFalse(check_changelog_fragment._is_title_feat_or_fix("docs: update README"))

    def test_none_does_not_match(self):
        self.assertFalse(check_changelog_fragment._is_title_feat_or_fix(None))

    def test_case_insensitive(self):
        self.assertTrue(check_changelog_fragment._is_title_feat_or_fix("FEAT: add widget"))


if __name__ == "__main__":
    unittest.main()

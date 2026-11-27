"""Tests for docs/redesign/tools/palette.py, the one source of the app's colours.

The app builds from two files the script writes (the theme JSON and
`ui/palette_gen.rs`); these tests fail when either drifts from `color.css`, and
when a contrast or colour-vision promise breaks.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import unittest

from support import REPO

PALETTE = REPO / "docs" / "redesign" / "tools" / "palette.py"


def load_palette():
    spec = importlib.util.spec_from_file_location("palette", PALETTE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class PaletteTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.palette = load_palette()

    def test_the_app_files_match_color_css(self):
        for path, text in self.palette.generated().items():
            with self.subTest(path=path):
                with open(path) as file:
                    self.assertEqual(file.read(), text,
                                     f"{path} is out of date: run palette.py --theme-json")

    def test_every_promised_pair_passes(self):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            self.palette.check(False)
        self.assertIn("All checks pass.", out.getvalue())

    def test_the_theme_has_four_variants_with_the_same_keys(self):
        themes = json.loads(self.palette.theme_json())["themes"]
        self.assertEqual([t["name"] for t in themes], [v[0] for v in self.palette.VARIANTS])
        keys = {frozenset(t["colors"]) for t in themes}
        self.assertEqual(len(keys), 1)

    def test_hover_and_pressed_fills_raise_label_contrast(self):
        p = self.palette
        for fill, label in (("#A71F43", "#FFFFFF"), ("#F58393", "#15141A")):
            hover = p.toward_label(fill, label, 0.04)
            pressed = p.toward_label(fill, label, 0.08)
            self.assertLess(p.cr(fill, label), p.cr(hover, label))
            self.assertLess(p.cr(hover, label), p.cr(pressed, label))

    def test_shadows_parse_into_layers(self):
        layers = self.palette.shadow_layers(
            {}, "0 1px 2px rgba(40, 32, 20, 0.05), 0 0 0 1px var(--border)", {"border": "#E4DFD6"})
        self.assertEqual(layers, [(0, 1, 2, 0, 0x2820140D), (0, 0, 0, 1, 0xE4DFD6FF)])
        self.assertEqual(self.palette.shadow_layers({}, "none", {}), [])


if __name__ == "__main__":
    unittest.main()

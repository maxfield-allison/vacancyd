from __future__ import annotations

from pathlib import Path
import re
import unittest
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
BRAND = ROOT / "docs" / "assets" / "brand"
COLORS = re.compile(r'(?:fill|stroke)="(#[0-9A-Fa-f]{6})"')
SVG = "{http://www.w3.org/2000/svg}"
CORE_COLORS = {
    "#0B0F17",
    "#151B27",
    "#FFB454",
    "#A85F06",
    "#94A3B8",
    "#55637A",
    "#E2603F",
    "#A83C22",
}
NIGHT_950 = "#0B0F17"
AMBER_400 = "#FFB454"
SLATE_600 = "#55637A"
WHITE = "#FFFFFF"


def relative_luminance(color: str) -> float:
    channels = [int(color[index:index + 2], 16) / 255 for index in (1, 3, 5)]
    linear = [value / 12.92 if value <= 0.04045 else ((value + 0.055) / 1.055) ** 2.4
              for value in channels]
    return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]


def contrast(first: str, second: str) -> float:
    light, dark = sorted((relative_luminance(first), relative_luminance(second)), reverse=True)
    return (light + 0.05) / (dark + 0.05)


def parts(name: str) -> tuple[ET.Element, ET.Element, ET.Element | None, ET.Element]:
    """The plate, the V, the rail group and the root of one mark."""
    root = ET.parse(BRAND / name).getroot()
    plate = root.find(f"{SVG}rect")
    letter = root.find(f"{SVG}path")
    assert plate is not None and letter is not None, name
    return plate, letter, root.find(f"{SVG}g[@id='rail']"), root


class BrandAssetTests(unittest.TestCase):
    def test_svg_literals_use_only_the_canonical_palette(self) -> None:
        for asset in BRAND.glob("*.svg"):
            with self.subTest(asset=asset.name):
                colors = set(COLORS.findall(asset.read_text(encoding="utf-8")))
                self.assertLessEqual(colors, CORE_COLORS)

    def test_the_lettering_is_the_light_source(self) -> None:
        # A motel sign lights its letters, not its plate. Every lit mark draws
        # the V in amber-400 on a night plate, and nothing else in it is amber.
        for name in ("mark-lit.svg", "mark-lit-deep.svg"):
            with self.subTest(asset=name):
                plate, letter, _, _ = parts(name)
                self.assertEqual(plate.get("fill"), NIGHT_950)
                self.assertEqual(letter.get("stroke"), AMBER_400)
                self.assertEqual((BRAND / name).read_text(encoding="utf-8").count(AMBER_400), 1)
                self.assertGreaterEqual(contrast(AMBER_400, NIGHT_950), 4.5)

    def test_each_lit_mark_reads_on_its_ground(self) -> None:
        dark_plate, _, dark_rail, _ = parts("mark-lit.svg")
        self.assertEqual(dark_plate.get("stroke"), SLATE_600)
        self.assertGreaterEqual(contrast(SLATE_600, NIGHT_950), 3)
        self.assertEqual(dark_rail.get("stroke"), SLATE_600)

        light_plate, _, light_rail, _ = parts("mark-lit-deep.svg")
        self.assertIsNone(light_plate.get("stroke"))
        self.assertGreaterEqual(contrast(NIGHT_950, WHITE), 3)
        self.assertEqual(light_rail.get("stroke"), SLATE_600)
        self.assertGreaterEqual(contrast(SLATE_600, WHITE), 3)

    def test_the_switched_off_sign_has_no_light(self) -> None:
        _, letter, _, _ = parts("mark-idle.svg")
        self.assertEqual(letter.get("stroke"), SLATE_600)
        self.assertNotIn(AMBER_400, (BRAND / "mark-idle.svg").read_text(encoding="utf-8"))
        self.assertGreaterEqual(contrast(SLATE_600, NIGHT_950), 3)

    def test_the_brand_mark_names_the_project_not_a_state(self) -> None:
        for name in ("mark-lit.svg", "mark-lit-deep.svg", "mark-mono.svg"):
            with self.subTest(asset=name):
                _, _, _, root = parts(name)
                self.assertEqual(root.get("aria-label"), "vacancyd")
                self.assertEqual(root.findtext(f"{SVG}title"), "vacancyd")

    def test_every_mark_has_a_rail_small_sizes_can_drop(self) -> None:
        # console/web/build-icon.mjs removes this group below 24px.
        for asset in BRAND.glob("*.svg"):
            with self.subTest(asset=asset.name):
                _, _, rail, _ = parts(asset.name)
                self.assertIsNotNone(rail)


if __name__ == "__main__":
    unittest.main()

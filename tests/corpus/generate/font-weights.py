#!/usr/bin/env python3
"""The weight fixtures, drawn from scratch: stems whose width is the weight.

Alpha2FixtureVariable.ttf, family "Alpha2 Fixture Variable": a variable font with a `wght` axis
from 100 to 900, default 400 (its usWeightClass). "H" is two stems and a bar, "I" one stem; a
stem is 100 units wide at 400, 40 at 100 and 260 at 900, in between linearly.

Alpha2FixturePair.ttc, family "Alpha2 Fixture Pair": a collection of two static faces, Regular
(usWeightClass 400, 100-unit stems) and Bold (700, 200-unit stems), with the same glyphs.

Every glyph sits on a 1000-unit em, 600 units wide. The output is the same bytes each run.
Needs fontTools (`pip install fonttools`).
Usage: font-weights.py <fonts directory>
"""

import io
import sys

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont
from fontTools.ttLib.tables.TupleVariation import TupleVariation
from fontTools.ttLib.ttCollection import TTCollection

ADVANCE = 600
CAP = 700


def rectangles(boxes):
    pen = TTGlyphPen(None)
    for left, bottom, right, top in boxes:
        pen.moveTo((left, bottom))
        pen.lineTo((left, top))
        pen.lineTo((right, top))
        pen.lineTo((right, bottom))
        pen.closePath()
    return pen.glyph()


def outlines(stem):
    """Each glyph's rectangles for a stem `stem` units wide, centred in the advance."""
    left, right = 100, ADVANCE - 100
    middle = ADVANCE // 2
    return {
        ".notdef": [(50, 0, ADVANCE - 50, CAP)],
        "space": [],
        "H": [
            (left, 0, left + stem, CAP),
            (right - stem, 0, right, CAP),
            (left, CAP // 2 - stem // 2, right, CAP // 2 + stem // 2),
        ],
        "I": [(middle - stem // 2, 0, middle + stem // 2, CAP)],
    }


ORDER = [".notdef", "space", "H", "I"]
CMAP = {0x20: "space", ord("H"): "H", ord("I"): "I"}


def face(family, style, weight, stem):
    builder = FontBuilder(1000, isTTF=True)
    builder.setupGlyphOrder(ORDER)
    builder.setupCharacterMap(CMAP)
    builder.setupGlyf({name: rectangles(boxes) for name, boxes in outlines(stem).items()})
    builder.setupHorizontalMetrics({name: (ADVANCE, 0) for name in ORDER})
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    compact = family.replace(" ", "")
    builder.setupNameTable(
        {
            "familyName": family,
            "styleName": style,
            "uniqueFontIdentifier": f"{compact}-{style}",
            "fullName": f"{family} {style}",
            "psName": f"{compact}-{style}",
        }
    )
    builder.setupOS2(
        sTypoAscender=800,
        sTypoDescender=-200,
        usWinAscent=800,
        usWinDescent=200,
        usWeightClass=weight,
    )
    builder.setupPost()
    builder.setupHead(unitsPerEm=1000, created=0, modified=0)
    return builder


def deltas(stem, default):
    """Each point's move from the `default` stem outlines to the `stem` ones, phantom points last."""
    moves = {}
    for name in ORDER:
        before = [point for box in outlines(default)[name] for point in corners(box)]
        after = [point for box in outlines(stem)[name] for point in corners(box)]
        moves[name] = [(x1 - x0, y1 - y0) for (x0, y0), (x1, y1) in zip(before, after)] + [(0, 0)] * 4
    return moves


def corners(box):
    left, bottom, right, top = box
    return [(left, bottom), (left, top), (right, top), (right, bottom)]


directory = sys.argv[1]

variable = face("Alpha2 Fixture Variable", "Regular", 400, 100)
variable.setupFvar(axes=[("wght", 100, 400, 900, "Weight")], instances=[])
thin, bold = deltas(40, 100), deltas(260, 100)
variable.setupGvar(
    {
        name: [
            TupleVariation({"wght": (-1.0, -1.0, 0.0)}, thin[name]),
            TupleVariation({"wght": (0.0, 1.0, 1.0)}, bold[name]),
        ]
        for name in ORDER
    }
)
variable.save(f"{directory}/Alpha2FixtureVariable.ttf")

collection = TTCollection()
for style, weight, stem in [("Regular", 400, 100), ("Bold", 700, 200)]:
    data = io.BytesIO()
    face("Alpha2 Fixture Pair", style, weight, stem).save(data)
    data.seek(0)
    collection.fonts.append(TTFont(data, recalcTimestamp=False))
collection.save(f"{directory}/Alpha2FixturePair.ttc")

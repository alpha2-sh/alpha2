#!/usr/bin/env python3
"""The style fixtures, drawn from scratch: one family with upright, bold and italic faces.

Alpha2FixtureStyles.ttf, Alpha2FixtureStylesBold.ttf and Alpha2FixtureStylesItalic.ttf, family
"Alpha2 Fixture Styles": Regular (usWeightClass 400, 100-unit stems, advance 600), Bold (700,
200-unit stems, advance 640) and Italic (400, stems slanted by 0.2, advance 560; fsSelection
italic, italicAngle -11.3). "H", "I", "T", "L", "E" and space. Each face sets its own `post`
underline and `OS/2` strikeout values:

    face     underline position, thickness   strikeout position, size
    Regular  -120, 50                        320, 50
    Bold     -130, 80                        330, 90
    Italic   -140, 60                        300, 60

Usage: font-styles.py <fonts directory>
"""

import sys

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen

CAP = 700
ORDER = [".notdef", "space", "H", "I", "T", "L", "E"]
CMAP = {0x20: "space", ord("H"): "H", ord("I"): "I", ord("T"): "T", ord("L"): "L", ord("E"): "E"}


def boxes(advance, stem):
    left, right = 80, advance - 80
    middle = advance // 2
    half = stem // 2
    bar = (CAP // 2 - half, CAP // 2 + half)
    return {
        ".notdef": [(50, 0, advance - 50, CAP)],
        "space": [],
        "H": [(left, 0, left + stem, CAP), (right - stem, 0, right, CAP), (left, bar[0], right, bar[1])],
        "I": [(middle - half, 0, middle + half, CAP)],
        "T": [(middle - half, 0, middle + half, CAP), (left, CAP - stem, right, CAP)],
        "L": [(left, 0, left + stem, CAP), (left, 0, right, stem)],
        "E": [
            (left, 0, left + stem, CAP),
            (left, 0, right, stem),
            (left, bar[0], right - stem, bar[1]),
            (left, CAP - stem, right, CAP),
        ],
    }


def glyph(rectangles, slant):
    pen = TTGlyphPen(None)
    for left, bottom, right, top in rectangles:
        corners = [(left, bottom), (left, top), (right, top), (right, bottom)]
        pen.moveTo(*[(round(x + slant * y), y) for x, y in corners[:1]])
        for x, y in corners[1:]:
            pen.lineTo((round(x + slant * y), y))
        pen.closePath()
    return pen.glyph()


def face(style, weight, advance, stem, slant, underline, strikeout):
    builder = FontBuilder(1000, isTTF=True)
    builder.setupGlyphOrder(ORDER)
    builder.setupCharacterMap(CMAP)
    builder.setupGlyf({name: glyph(rects, slant) for name, rects in boxes(advance, stem).items()})
    builder.setupHorizontalMetrics({name: (advance, 0) for name in ORDER})
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    family = "Alpha2 Fixture Styles"
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
    italic = slant != 0
    builder.setupOS2(
        sTypoAscender=800,
        sTypoDescender=-200,
        usWinAscent=800,
        usWinDescent=200,
        usWeightClass=weight,
        fsSelection=(1 << 0) if italic else (1 << 5 if weight >= 700 else 1 << 6),
        yStrikeoutPosition=strikeout[0],
        yStrikeoutSize=strikeout[1],
    )
    builder.setupPost(
        italicAngle=-11.3 if italic else 0,
        underlinePosition=underline[0],
        underlineThickness=underline[1],
    )
    builder.setupHead(unitsPerEm=1000, created=0, modified=0, macStyle=(2 if italic else 0) | (1 if weight >= 700 else 0))
    return builder


directory = sys.argv[1]
face("Regular", 400, 600, 100, 0.0, (-120, 50), (320, 50)).save(f"{directory}/Alpha2FixtureStyles.ttf")
face("Bold", 700, 640, 200, 0.0, (-130, 80), (330, 90)).save(f"{directory}/Alpha2FixtureStylesBold.ttf")
face("Italic", 400, 560, 100, 0.2, (-140, 60), (300, 60)).save(f"{directory}/Alpha2FixtureStylesItalic.ttf")

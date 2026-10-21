#!/usr/bin/env python3
"""Alpha2FixtureColor.ttf: a COLR font drawn from scratch, a glyph for each kind of painting.

Every glyph sits on a 1000-unit em, with outlines of squares and an octagon:

  U+E000  version 0: a square in palette red, an octagon in the text's color (0xFFFF), a bar in
          half-transparent blue; its base glyph has the square's outline for monochrome drawing
  U+E001  linear gradient (pad), red to blue, on the square
  U+E002  radial gradient (reflect), yellow to green to transparent, on the square
  U+E003  sweep gradient (repeat), red to green to blue, on the octagon
  U+E004  composite: the octagon in green multiplied onto the square in red, in a clip box
  U+E005  transform: the bar rotated 30 degrees about the centre, scaled and moved, in the text's
          color at half alpha

The output is the same bytes each run. Needs fontTools (`pip install fonttools`).
Usage: font-color.py <fonts directory>
"""

import sys

from fontTools.colorLib.builder import buildCOLR, buildCPAL
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib.tables.otTables import CompositeMode, ExtendMode, PaintFormat


def polygon(points):
    pen = TTGlyphPen(None)
    pen.moveTo(points[0])
    for point in points[1:]:
        pen.lineTo(point)
    pen.closePath()
    return pen.glyph()


def empty():
    return TTGlyphPen(None).glyph()


SQUARE = [(100, 0), (100, 800), (900, 800), (900, 0)]
OCTAGON = [(350, 50), (150, 250), (150, 550), (350, 750), (650, 750), (850, 550), (850, 250), (650, 50)]
BAR = [(150, 300), (150, 500), (850, 500), (850, 300)]

glyphs = {
    ".notdef": polygon(SQUARE),
    "space": empty(),
    "square": polygon(SQUARE),
    "octagon": polygon(OCTAGON),
    "bar": polygon(BAR),
    "layers": polygon(SQUARE),
    "linear": empty(),
    "radial": empty(),
    "sweep": empty(),
    "composite": empty(),
    "transform": empty(),
}
order = list(glyphs)
cmap = {
    0x20: "space",
    0xE000: "layers",
    0xE001: "linear",
    0xE002: "radial",
    0xE003: "sweep",
    0xE004: "composite",
    0xE005: "transform",
}

# Palette 0: red, blue at half alpha, yellow, green, transparent green.
palette = [
    (1.0, 0.0, 0.0, 1.0),
    (0.0, 0.0, 1.0, 0.5),
    (1.0, 0.85, 0.0, 1.0),
    (0.0, 0.6, 0.2, 1.0),
    (0.0, 0.6, 0.2, 0.0),
    (0.0, 0.0, 1.0, 1.0),
]
RED, HALF_BLUE, YELLOW, GREEN, CLEAR_GREEN, BLUE = range(6)
FOREGROUND = 0xFFFF


def glyph(name, paint):
    return {"Format": PaintFormat.PaintGlyph, "Glyph": name, "Paint": paint}


def solid(index, alpha=1.0):
    return {"Format": PaintFormat.PaintSolid, "PaletteIndex": index, "Alpha": alpha}


def line(extend, stops):
    return {
        "Extend": extend,
        "ColorStop": [{"StopOffset": offset, "PaletteIndex": index, "Alpha": 1.0} for offset, index in stops],
    }


colors = {
    "layers": [("square", RED), ("octagon", FOREGROUND), ("bar", HALF_BLUE)],
    "linear": glyph(
        "square",
        {
            "Format": PaintFormat.PaintLinearGradient,
            "ColorLine": line(ExtendMode.PAD, [(0.0, RED), (1.0, BLUE)]),
            "x0": 200,
            "y0": 100,
            "x1": 800,
            "y1": 700,
            "x2": 200,
            "y2": 700,
        },
    ),
    "radial": glyph(
        "square",
        {
            "Format": PaintFormat.PaintRadialGradient,
            "ColorLine": line(ExtendMode.REFLECT, [(0.0, YELLOW), (0.5, GREEN), (1.0, CLEAR_GREEN)]),
            "x0": 400,
            "y0": 450,
            "r0": 0,
            "x1": 500,
            "y1": 400,
            "r1": 250,
        },
    ),
    "sweep": glyph(
        "octagon",
        {
            "Format": PaintFormat.PaintSweepGradient,
            "ColorLine": line(ExtendMode.REPEAT, [(0.0, RED), (0.5, GREEN), (1.0, BLUE)]),
            "centerX": 500,
            "centerY": 400,
            "startAngle": 0.0,
            "endAngle": 180.0,
        },
    ),
    "composite": {
        "Format": PaintFormat.PaintComposite,
        "SourcePaint": glyph("octagon", solid(GREEN)),
        "CompositeMode": CompositeMode.MULTIPLY,
        "BackdropPaint": glyph("square", solid(RED)),
    },
    "transform": {
        "Format": PaintFormat.PaintTranslate,
        "dx": 0,
        "dy": 50,
        "Paint": {
            "Format": PaintFormat.PaintScaleAroundCenter,
            "scaleX": 1.0,
            "scaleY": 0.75,
            "centerX": 500,
            "centerY": 400,
            "Paint": {
                "Format": PaintFormat.PaintRotateAroundCenter,
                "angle": 30.0,
                "centerX": 500,
                "centerY": 400,
                "Paint": glyph("bar", solid(FOREGROUND, 0.5)),
            },
        },
    },
}

builder = FontBuilder(1000, isTTF=True)
builder.setupGlyphOrder(order)
builder.setupCharacterMap(cmap)
builder.setupGlyf(glyphs)
builder.setupHorizontalMetrics({name: (1000, 0) for name in order})
builder.setupHorizontalHeader(ascent=800, descent=-200)
builder.setupNameTable(
    {
        "familyName": "Alpha2 Fixture Color",
        "styleName": "Regular",
        "uniqueFontIdentifier": "Alpha2FixtureColor-Regular",
        "fullName": "Alpha2 Fixture Color Regular",
        "psName": "Alpha2FixtureColor-Regular",
    }
)
builder.setupOS2(sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200)
builder.setupPost()
builder.setupHead(unitsPerEm=1000, created=0, modified=0)
builder.font["COLR"] = buildCOLR(
    colors,
    glyphMap=builder.font.getReverseGlyphMap(),
    clipBoxes={"composite": (100, 0, 800, 800)},
)
builder.font["CPAL"] = buildCPAL([palette])
builder.save(f"{sys.argv[1]}/Alpha2FixtureColor.ttf")

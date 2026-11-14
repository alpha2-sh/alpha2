#!/usr/bin/env python3
"""Hostile font fixtures: each is small and asks the text host for unbounded work.

  Alpha2HostileComposite.ttf  "A" is a composite chain 24 deep, each level naming the next twice
                              (2^24 leaf contours)
  Alpha2HostilePaint.ttf      "A" is a COLR v1 painting of PaintColrLayers 4 deep, 255 layers per
                              level over one shared level below (255^4 leaf paints)
  Alpha2HostileStrike.ttf     "A" is a 20 ppem sbix strike holding a 4096 x 4096 PNG, the largest
                              the PNG decoder's own 64 MiB limit admits
  Alpha2HostileCollection.ttc a collection of FACES faces (default 1000), all one small face

Usage: font-hostile.py <fonts directory> [FACES]
"""

import io
import struct
import sys
import zlib

from fontTools.colorLib.builder import buildCOLR, buildCPAL
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib.tables import _g_l_y_f
from fontTools.ttLib.tables import otTables as ot
from fontTools.ttLib.tables._s_b_i_x import table__s_b_i_x
from fontTools.ttLib.tables.sbixGlyph import Glyph as SbixGlyph
from fontTools.ttLib.tables.sbixStrike import Strike as SbixStrike

SQUARE = [(100, 0), (100, 700), (700, 700), (700, 0)]

# fontTools computes a composite's bounds by walking every leaf unless it is given a set of
# glyphs already done; always give it one.
recalc_bounds = _g_l_y_f.Glyph.recalcBounds
_g_l_y_f.Glyph.recalcBounds = lambda self, table, *, boundsDone=None: recalc_bounds(
    self, table, boundsDone=set() if boundsDone is None else boundsDone
)


def square():
    pen = TTGlyphPen(None)
    pen.moveTo(SQUARE[0])
    for point in SQUARE[1:]:
        pen.lineTo(point)
    pen.closePath()
    return pen.glyph()


def font(name, glyphs, cmap):
    order = list(glyphs)
    builder = FontBuilder(1000, isTTF=True)
    builder.setupGlyphOrder(order)
    builder.setupCharacterMap(cmap)
    builder.setupGlyf(glyphs)
    # maxp's composite counts walk every leaf; the text host never reads them.
    builder.font.recalcBBoxes = False
    builder.setupHorizontalMetrics({glyph: (800, 0) for glyph in order})
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    builder.setupNameTable({"familyName": name, "styleName": "Regular"})
    builder.setupOS2(sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200)
    builder.setupPost()
    builder.setupHead(unitsPerEm=1000, created=0, modified=0)
    return builder


def save(builder):
    data = io.BytesIO()
    builder.save(data)
    return data.getvalue()


def composite():
    glyphs = {".notdef": TTGlyphPen(None).glyph(), "g0": square()}
    for level in range(1, 25):
        pen = TTGlyphPen(glyphs)
        pen.addComponent(f"g{level - 1}", (1, 0, 0, 1, 0, 0))
        pen.addComponent(f"g{level - 1}", (1, 0, 0, 1, 1, 0))
        glyphs[f"g{level}"] = pen.glyph()
    return save(font("Alpha2 Hostile Composite", glyphs, {0x41: "g24"}))


def layers(first, count):
    paint = ot.Paint()
    paint.Format = 1  # PaintColrLayers
    paint.NumLayers = count
    paint.FirstLayerIndex = first
    return paint


def paint_graph():
    builder = font(
        "Alpha2 Hostile Paint",
        {".notdef": TTGlyphPen(None).glyph(), "square": square(), "bomb": TTGlyphPen(None).glyph()},
        {0x41: "bomb"},
    )
    leaf = {"Format": 10, "Glyph": "square", "Paint": {"Format": 2, "PaletteIndex": 0, "Alpha": 1.0}}
    colr = buildCOLR({"bomb": leaf}, glyphMap=builder.font.getReverseGlyphMap())
    table = colr.table
    leaf_paint = table.BaseGlyphList.BaseGlyphPaintRecord[0].Paint
    # Level 0 is 255 copies of the leaf; level k is 255 references to all of level k - 1.
    paints = [leaf_paint] * 255
    for level in range(1, 4):
        paints += [layers(255 * (level - 1), 255)] * 255
    table.LayerList = ot.LayerList()
    table.LayerList.Paint = paints
    table.LayerList.LayerCount = len(paints)
    table.BaseGlyphList.BaseGlyphPaintRecord[0].Paint = layers(255 * 3, 255)
    builder.font["COLR"] = colr
    builder.font["CPAL"] = buildCPAL([[(1.0, 0.0, 0.0, 1.0)]])
    return save(builder)


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def strike():
    builder = font(
        "Alpha2 Hostile Strike",
        {".notdef": TTGlyphPen(None).glyph(), "big": TTGlyphPen(None).glyph()},
        {0x41: "big"},
    )
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 4096, 4096, 8, 0, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"\x00" * (4097 * 4096), 9))
        + chunk(b"IEND", b"")
    )
    sbix = table__s_b_i_x()
    sbix.version = 1
    sbix.flags = 1
    one = SbixStrike(ppem=20, resolution=72)
    one.glyphs[".notdef"] = SbixGlyph(glyphName=".notdef")
    one.glyphs["big"] = SbixGlyph(glyphName="big", graphicType="png ", imageData=png)
    sbix.strikes[20] = one
    builder.font["sbix"] = sbix
    return save(builder)


def collection(faces):
    face = save(font("Alpha2 Hostile Face", {".notdef": TTGlyphPen(None).glyph(), "A": square()}, {0x41: "A"}))
    header = 12 + 4 * faces
    # Table offsets in a collection count from the start of the file.
    face = bytearray(face)
    for record in range(struct.unpack(">H", face[4:6])[0]):
        at = 12 + 16 * record + 8
        struct.pack_into(">I", face, at, struct.unpack(">I", face[at : at + 4])[0] + header)
    return b"ttcf" + struct.pack(">HHI", 1, 0, faces) + struct.pack(">I", header) * faces + bytes(face)


directory = sys.argv[1]
faces = int(sys.argv[2]) if len(sys.argv) > 2 else 1000
for name, data in [
    ("Alpha2HostileComposite.ttf", composite()),
    ("Alpha2HostilePaint.ttf", paint_graph()),
    ("Alpha2HostileStrike.ttf", strike()),
    ("Alpha2HostileCollection.ttc", collection(faces)),
]:
    with open(f"{directory}/{name}", "wb") as file:
        file.write(data)

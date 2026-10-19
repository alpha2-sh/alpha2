"""Writes the OpenEXR corpus from the file format's specification alone
(uncompressed scanline files), so the Rust decoder is checked against an
independent writer. Each file's pixels are known exactly:

- float.exr: 4x2 RGBA 32-bit float, values above 1.0, negative, and
  half-transparent (premultiplied, as OpenEXR stores alpha);
- half.exr: the same pixels as 16-bit half floats;
- window.exr: RGB half, a 2x2 data window at (1, 1) inside a 4x3 display
  window, the rest of the display window transparent;
- nonfinite.exr: RGB float with a NaN, a +Inf, and a -Inf;
- sequence_0001.exr, sequence_0002.exr: two 4x2 half frames.
"""

import struct

FLOAT, HALF = 2, 1


def attribute(name, kind, value):
    return name.encode() + b"\0" + kind.encode() + b"\0" + struct.pack("<i", len(value)) + value


def box2i(x0, y0, x1, y1):
    return struct.pack("<4i", x0, y0, x1, y1)


def channels(names, pixel_type):
    body = b""
    for name in sorted(names):
        body += name.encode() + b"\0" + struct.pack("<i", pixel_type) + b"\0\0\0\0" + struct.pack("<ii", 1, 1)
    return body + b"\0"


def half(value):
    return struct.pack("<e", value)


def write(path, names, pixel_type, data_window, display_window, rows):
    """`rows` maps each data-window row to {channel: [values]}."""
    x0, y0, x1, y1 = data_window
    header = b"\x76\x2f\x31\x01" + struct.pack("<i", 2)
    header += attribute("channels", "chlist", channels(names, pixel_type))
    header += attribute("compression", "compression", b"\0")
    header += attribute("dataWindow", "box2i", box2i(*data_window))
    header += attribute("displayWindow", "box2i", box2i(*display_window))
    header += attribute("lineOrder", "lineOrder", b"\0")
    header += attribute("pixelAspectRatio", "float", struct.pack("<f", 1.0))
    header += attribute("screenWindowCenter", "v2f", struct.pack("<ff", 0.0, 0.0))
    header += attribute("screenWindowWidth", "float", struct.pack("<f", 1.0))
    header += b"\0"
    pack = (lambda v: struct.pack("<f", v)) if pixel_type == FLOAT else half
    lines = []
    for y in range(y0, y1 + 1):
        data = b"".join(pack(v) for name in sorted(names) for v in rows[y][name])
        lines.append(struct.pack("<ii", y, len(data)) + data)
    table_start = len(header)
    offsets, at = [], table_start + 8 * len(lines)
    for line in lines:
        offsets.append(at)
        at += len(line)
    with open(path, "wb") as out:
        out.write(header + b"".join(struct.pack("<Q", o) for o in offsets) + b"".join(lines))


# Straight (r, g, b, a) to premultiplied, as OpenEXR stores RGBA.
STRAIGHT = [(2.0, 0.5, -0.25, 1.0), (0.0, 0.0, 0.0, 0.0), (1.0, 1.0, 1.0, 0.5), (4.0, 0.25, 0.125, 1.0)]
PIXELS = [(r * a, g * a, b * a, a) for (r, g, b, a) in STRAIGHT]


def rgba_rows(width, height):
    rows = {}
    for y in range(height):
        row = [PIXELS[(x + y) % 4] for x in range(width)]
        rows[y] = {c: [p[i] for p in row] for i, c in enumerate("RGBA")}
    return rows


write("float.exr", "RGBA", FLOAT, (0, 0, 3, 1), (0, 0, 3, 1), rgba_rows(4, 2))
write("half.exr", "RGBA", HALF, (0, 0, 3, 1), (0, 0, 3, 1), rgba_rows(4, 2))
write(
    "window.exr",
    "RGB",
    HALF,
    (1, 1, 2, 2),
    (0, 0, 3, 2),
    {1: {"R": [1.0, 0.5], "G": [0.25, 0.5], "B": [0.0, 2.0]}, 2: {"R": [0.0, 1.0], "G": [1.0, 0.0], "B": [0.5, 0.5]}},
)
nan, inf = float("nan"), float("inf")
write("nonfinite.exr", "RGB", FLOAT, (0, 0, 1, 0), (0, 0, 1, 0), {0: {"R": [nan, 0.5], "G": [inf, 0.5], "B": [-inf, 0.5]}})
for index, value in enumerate([0.25, 0.75]):
    write(
        f"sequence_{index + 1:04d}.exr",
        "RGB",
        HALF,
        (0, 0, 3, 1),
        (0, 0, 3, 1),
        {y: {c: [value] * 4 for c in "RGB"} for y in range(2)},
    )

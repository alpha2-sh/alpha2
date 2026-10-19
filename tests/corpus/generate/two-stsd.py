#!/usr/bin/env python3
"""Writes an MP4 whose video track switches sample description halfway.

Usage: two-stsd.py <first.mp4> <second.mp4> <out.mp4>

Both inputs are the same video-only MP4 whose `moov` follows its `mdat`,
with one chunk of samples, except for their `avc1` entry (parameter sets):
the second's was rewritten (`h264_metadata`). The output is the first file
with the second's entry added to `stsd`, its samples split into two chunks
at the middle, and the second chunk described by the second entry, as an
editor that joins recordings without re-encoding writes them. FFmpeg reads
the first entry's parameter sets as the stream's and marks the packet where
the second begins with new extradata.
"""

import struct
import sys


def boxes(data, start, end):
    at = start
    while at < end:
        size, kind = struct.unpack_from(">I4s", data, at)
        yield at, size, kind
        at += size


def find(data, start, end, path):
    for at, size, kind in boxes(data, start, end):
        if kind == path[0]:
            if len(path) == 1:
                return at, size
            return find(data, at + (16 if kind == b"stsd" else 8), at + size, path[1:])
    raise SystemExit(f"missing {path}")


STBL = [b"moov", b"trak", b"mdia", b"minf", b"stbl"]
first = bytearray(open(sys.argv[1], "rb").read())
second = open(sys.argv[2], "rb").read()
assert first.find(b"mdat") < first.find(b"moov")
entry_at, entry_size = find(second, 0, len(second), STBL + [b"stsd", b"avc1"])
entry = second[entry_at : entry_at + entry_size]

at, size = find(first, 0, len(first), STBL + [b"stsz"])
sample_size, count = struct.unpack_from(">II", first, at + 12)
assert sample_size == 0 and count % 2 == 0
sizes = struct.unpack_from(f">{count}I", first, at + 20)
at, size = find(first, 0, len(first), STBL + [b"stco"])
assert struct.unpack_from(">I", first, at + 12)[0] == 1
offset = struct.unpack_from(">I", first, at + 16)[0]
half = count // 2
stco = struct.pack(">I4sII", 24, b"stco", 0, 2) + struct.pack(">II", offset, offset + sum(sizes[:half]))
stsc = struct.pack(">I4sII", 40, b"stsc", 0, 2) + struct.pack(">III", 1, half, 1) + struct.pack(">III", 2, half, 2)

# Replace the tables, last first so earlier offsets hold, and grow every box around them.
grown = 0
for kind, table in [(b"stco", stco), (b"stsc", stsc)]:
    at, size = find(first, 0, len(first), STBL + [kind])
    first[at : at + size] = table
    grown += len(table) - size
at, size = find(first, 0, len(first), STBL + [b"stsd"])
first[at + size : at + size] = entry
struct.pack_into(">I", first, at, size + len(entry))
struct.pack_into(">I", first, at + 12, 2)
grown += len(entry)
for depth in range(len(STBL), 0, -1):
    at, size = find(first, 0, len(first), STBL[:depth])
    struct.pack_into(">I", first, at, size + grown)
open(sys.argv[3], "wb").write(first)

#!/usr/bin/env python3
"""Repaginates an Ogg Opus file so its audio packets continue across pages.

Usage: ogg-continued.py <input.ogg> <output.ogg>

Each audio page holds two lacing segments, so packets of 255 bytes or more span pages and some
pages end no packet (granule position -1). Header pages and the final granule position are kept.
"""

import struct
import sys

SEGMENTS = 2


def crc32(data):
    crc = 0
    for byte in data:
        crc ^= byte << 24
        for _ in range(8):
            crc = ((crc << 1) ^ 0x04C11DB7) if crc & 0x80000000 else crc << 1
            crc &= 0xFFFFFFFF
    return crc


def pages(data):
    pos = 0
    while pos < len(data):
        flags, granule, serial = struct.unpack("<BqI", data[pos + 5 : pos + 18])
        count = data[pos + 26]
        lacing = data[pos + 27 : pos + 27 + count]
        body = pos + 27 + count
        yield flags, granule, serial, lacing, data[body : body + sum(lacing)]
        pos = body + sum(lacing)


def packets(lacing, body):
    out, start, end = [], 0, 0
    for value in lacing:
        end += value
        if value < 255:
            out.append(body[start:end])
            start = end
    return out


def page(flags, granule, serial, sequence, lacing, body):
    header = b"OggS" + struct.pack("<BBqIIIB", 0, flags, granule, serial, sequence, 0, len(lacing))
    raw = header + bytes(lacing) + body
    return raw[:22] + struct.pack("<I", crc32(raw)) + raw[26:]


def duration(packet):
    toc = packet[0]
    config, count = toc >> 3, toc & 3
    if config < 12:
        frame = max(480, 960 * (config & 3))
    elif config < 16:
        frame = 480 << (config & 1)
    else:
        frame = 120 << (config & 3)
    return frame * (1 if count == 0 else 2 if count < 3 else packet[1] & 0x3F)


source = list(pages(open(sys.argv[1], "rb").read()))
serial = source[0][2]
final = source[-1][1]
audio = [p for _, _, _, lacing, body in source[2:] for p in packets(lacing, body)]

out = [page(f, g, serial, n, lacing, body) for n, (f, g, _, lacing, body) in enumerate(source[:2])]
segments = []
end_sample = 0
for packet in audio:
    end_sample += duration(packet)
    lacing = [255] * (len(packet) // 255) + [len(packet) % 255]
    offset = 0
    for value in lacing:
        segments.append((value, packet[offset : offset + value], end_sample if value < 255 else None))
        offset += value
continued = False
for first in range(0, len(segments), SEGMENTS):
    chunk = segments[first : first + SEGMENTS]
    ended = [sample for _, _, sample in chunk if sample is not None]
    last = first + SEGMENTS >= len(segments)
    flags = (1 if continued else 0) | (4 if last else 0)
    granule = final if last else ended[-1] if ended else -1
    lacing = [value for value, _, _ in chunk]
    body = b"".join(data for _, data, _ in chunk)
    out.append(page(flags, granule, serial, len(out), lacing, body))
    continued = chunk[-1][0] == 255
open(sys.argv[2], "wb").write(b"".join(out))

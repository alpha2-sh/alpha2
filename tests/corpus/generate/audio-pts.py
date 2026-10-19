#!/usr/bin/env python3
"""Writes two Matroska files whose audio timestamps no real recording has.

Usage: audio-pts.py <ahead.mkv> <behind.mkv>

Both carry 16-bit mono PCM at 44.1 kHz, a 440 Hz tone, in 1024-sample blocks
of unknown-size clusters with 8-byte timestamps (1 ms time scale):

- ahead: ten blocks from 0 ms, then ten more in a cluster 3e17 ms later.
- behind: ten blocks in a cluster whose unsigned timestamp wraps to 3e17 ms
  before zero, then forty from 0 ms.

At 44.1 kHz a time 3e17 ms from zero is past what a 64-bit sample index
holds, so a decoder that places samples by their timestamps has to refuse
these rather than compute with them.
"""

import math
import struct
import sys

RATE = 44_100
FRAME = 1_024
FAR_MS = 300_000_000_000_000_000
UNKNOWN_SIZE = b"\x01\xff\xff\xff\xff\xff\xff\xff"


def size(value):
    for length in range(1, 9):
        if value < (1 << (7 * length)) - 1:
            return bytes([(1 << (8 - length)) | (value >> (8 * (length - 1)))]) + value.to_bytes(length, "big")[1:]
    raise ValueError(value)


def element(ident, payload):
    return ident + size(len(payload)) + payload


def uint(ident, value, width=None):
    return element(ident, value.to_bytes(width or max(1, (value.bit_length() + 7) // 8), "big"))


def tone(first, count):
    return b"".join(
        struct.pack("<h", round(8000 * math.sin(2 * math.pi * 440 * (first + n) / RATE))) for n in range(count)
    )


def cluster(timestamp, first, blocks):
    body = uint(b"\xe7", timestamp, 8)
    for block in range(blocks):
        offset = block * FRAME * 1000 // RATE
        body += element(b"\xa3", b"\x81" + struct.pack(">h", offset) + b"\x80" + tone(first + block * FRAME, FRAME))
    return b"\x1f\x43\xb6\x75" + UNKNOWN_SIZE + body


def write(path, clusters):
    header = element(
        b"\x1a\x45\xdf\xa3",
        uint(b"\x42\x86", 1)
        + uint(b"\x42\xf7", 1)
        + uint(b"\x42\xf2", 4)
        + uint(b"\x42\xf3", 8)
        + element(b"\x42\x82", b"matroska")
        + uint(b"\x42\x87", 4)
        + uint(b"\x42\x85", 2),
    )
    info = element(b"\x15\x49\xa9\x66", uint(b"\x2a\xd7\xb1", 1_000_000) + element(b"\x4d\x80", b"alpha2") + element(b"\x57\x41", b"alpha2"))
    audio = element(b"\xe1", element(b"\xb5", struct.pack(">d", RATE)) + uint(b"\x9f", 1) + uint(b"\x62\x64", 16))
    track = element(b"\xae", uint(b"\xd7", 1) + uint(b"\x73\xc5", 1) + uint(b"\x83", 2) + element(b"\x86", b"A_PCM/INT/LIT") + audio)
    segment = b"\x18\x53\x80\x67" + UNKNOWN_SIZE + info + element(b"\x16\x54\xae\x6b", track) + b"".join(clusters)
    with open(path, "wb") as file:
        file.write(header + segment)


write(sys.argv[1], [cluster(0, 0, 10), cluster(FAR_MS, 10 * FRAME, 10)])
write(sys.argv[2], [cluster((1 << 64) - FAR_MS, 0, 10), cluster(0, 10 * FRAME, 20)])

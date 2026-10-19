#!/usr/bin/env python3
"""Writes an MP4 whose H.264 SPS has no VUI.

Usage: no-vui.py <in.mp4> <out.mp4>

The input is a video-only MP4 whose `moov` follows its `mdat` and whose only
SPS is the one in `avcC`. The SPS is cut after `vui_parameters_present_flag`,
which is written as 0, so the stream declares no VUI at all: H.264 E.2.1 then
infers every VUI field, among them chroma sample location type 0 (left),
where FFmpeg's decoder reports the location unspecified.
"""

import struct
import sys

def unescape(nal):
    out, zeros = bytearray(), 0
    for byte in nal:
        if zeros >= 2 and byte == 3:
            zeros = 0
            continue
        out.append(byte)
        zeros = zeros + 1 if byte == 0 else 0
    return bytes(out)

def escape(rbsp):
    out, zeros = bytearray(), 0
    for byte in rbsp:
        if zeros >= 2 and byte <= 3:
            out.append(3)
            zeros = 0
        out.append(byte)
        zeros = zeros + 1 if byte == 0 else 0
    return bytes(out)

class Bits:
    def __init__(self, data):
        self.data, self.pos = data, 0
    def bit(self):
        value = (self.data[self.pos // 8] >> (7 - self.pos % 8)) & 1
        self.pos += 1
        return value
    def bits(self, count):
        value = 0
        for _ in range(count):
            value = value << 1 | self.bit()
        return value
    def ue(self):
        zeros = 0
        while self.bit() == 0:
            zeros += 1
        return (1 << zeros) - 1 + self.bits(zeros)
    def se(self):
        value = self.ue()
        return (value + 1) // 2 if value & 1 else -(value // 2)

def strip(nal):
    """The SPS NAL unit `nal` without its VUI."""
    rbsp = unescape(nal)
    bits = Bits(rbsp[1:])
    profile = bits.bits(8)
    bits.bits(16)
    bits.ue()
    if profile in (100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 144):
        chroma = bits.ue()
        if chroma == 3:
            bits.bit()
        bits.ue(); bits.ue(); bits.bit()
        assert bits.bit() == 0, "no scaling matrix"
    bits.ue()
    poc = bits.ue()
    if poc == 0:
        bits.ue()
    elif poc == 1:
        bits.bit(); bits.se(); bits.se()
        for _ in range(bits.ue()):
            bits.se()
    bits.ue(); bits.bit(); bits.ue(); bits.ue()
    if bits.bit() == 0:
        bits.bit()
    bits.bit()
    if bits.bit():
        for _ in range(4):
            bits.ue()
    flag = bits.pos
    assert bits.bit() == 1, "the SPS has a VUI"
    # Keep everything before vui_parameters_present_flag, write it as 0, then the stop bit.
    out = Bits(rbsp[1:])
    kept = [out.bit() for _ in range(flag)] + [0, 1]
    while len(kept) % 8:
        kept.append(0)
    body = bytes(int("".join(map(str, kept[i:i + 8])), 2) for i in range(0, len(kept), 8))
    return escape(rbsp[:1] + body)

data = bytearray(open(sys.argv[1], "rb").read())
# Every box on the path to `avcC` grows with it; the `moov` follows the `mdat`, so no chunk offset
# moves.
path = []
def walk(start, end):
    while start < end:
        size, kind = struct.unpack(">I4s", data[start:start + 8])
        if kind == b"avcC":
            return start, size
        body = {b"moov": 8, b"trak": 8, b"mdia": 8, b"minf": 8, b"stbl": 8, b"stsd": 16, b"avc1": 86}.get(kind)
        if body is not None:
            path.append(start)
            found = walk(start + body, start + size)
            if found:
                return found
            path.pop()
        start += size
    return None
at, size = walk(0, len(data))
record = data[at + 8:at + size]
assert record[0] == 1 and record[5] & 0x1F == 1
length = record[6] << 8 | record[7]
sps = strip(bytes(record[8:8 + length]))
record = record[:6] + struct.pack(">H", len(sps)) + sps + record[8 + length:]
delta = len(record) + 8 - size
data[at:at + size] = struct.pack(">I4s", len(record) + 8, b"avcC") + record
for box in path:
    struct.pack_into(">I", data, box, struct.unpack(">I", data[box:box + 4])[0] + delta)
open(sys.argv[2], "wb").write(data)

#!/usr/bin/env python3
"""Writes two Matroska files with the block layouts FFmpeg's muxer never writes.

Usage: matroska-blocks.py <opus.webm> <mpeg4.mkv> <laced.mkv> <groups.mkv>

laced: the Opus packets of <opus.webm> Xiph-laced four to a SimpleBlock without durations, and
EBML-laced three to a BlockGroup with a duration and the last one's DiscardPadding; PCM
fixed-laced with a DefaultDuration; Cues and a SeekHead.

groups: the MPEG-4 Part 2 frames of <mpeg4.mkv> repeated six times, in BlockGroups with
ReferenceBlock and some BlockDurations, again as SimpleBlocks with the start code stripped by a
ContentEncoding, and big-endian PCM EBML-laced without durations; a 100 us TimestampScale,
unknown sizes, no Cues, Void, CRC-32 and unknown elements, and two track entries FFmpeg skips.
"""

import math
import struct
import sys
import zlib

UNKNOWN_SIZE = b"\x01\xff\xff\xff\xff\xff\xff\xff"


def size(value):
    for length in range(1, 9):
        if value < (1 << (7 * length)) - 1:
            return bytes([(1 << (8 - length)) | (value >> (8 * (length - 1)))]) + value.to_bytes(length, "big")[1:]
    raise ValueError(value)


def element(ident, payload):
    return ident + size(len(payload)) + payload


def uint(ident, value):
    return element(ident, value.to_bytes(max(1, (value.bit_length() + 7) // 8), "big"))


def sint(ident, value):
    length = 1
    while not -(1 << (8 * length - 1)) <= value < (1 << (8 * length - 1)):
        length += 1
    return element(ident, value.to_bytes(length, "big", signed=True))


def double(ident, value):
    return element(ident, struct.pack(">d", value))


def crc(payload):
    return element(b"\xbf", struct.pack("<I", zlib.crc32(payload))) + payload


def vint(data, at, keep_marker):
    first = data[at]
    length = 1
    while not first & (0x80 >> (length - 1)):
        length += 1
    value = first if keep_marker else first & (0xFF >> length)
    for byte in data[at + 1 : at + length]:
        value = (value << 8) | byte
    return value, length


def children(data, start, end):
    at = start
    while at < end:
        ident, id_length = vint(data, at, True)
        length, size_length = vint(data, at + id_length, False)
        body = at + id_length + size_length
        yield ident, body, min(body + length, end)
        at = body + length


def find(data, start, end, ident):
    return next((body, stop) for found, body, stop in children(data, start, end) if found == ident)


def source(path, number):
    """Track `number`'s codec private data, CodecDelay, and blocks (timestamp, frame)."""
    data = open(path, "rb").read()
    segment = find(data, 0, len(data), 0x18538067)
    tracks = find(data, *segment, 0x1654AE6B)
    entries = [(body, stop) for ident, body, stop in children(data, *tracks) if ident == 0xAE]
    fields = {}
    for entry in entries:
        found = {ident: data[body:stop] for ident, body, stop in children(data, *entry)}
        if int.from_bytes(found[0xD7], "big") == number:
            fields = found
    blocks = []
    for ident, body, stop in children(data, *segment):
        if ident != 0x1F43B675:
            continue
        cluster = 0
        for child, start, end in children(data, body, stop):
            if child == 0xE7:
                cluster = int.from_bytes(data[start:end], "big")
            group = child == 0xA0
            if group:
                start, end = find(data, start, end, 0xA1)
            if child in (0xA3, 0xA0):
                track, length = vint(data, start, False)
                if track == number:
                    relative = struct.unpack(">h", data[start + length : start + length + 2])[0]
                    blocks.append((cluster + relative, data[start + length + 3 : end]))
    return fields.get(0x63A2, b""), int.from_bytes(fields.get(0x56AA, b""), "big"), blocks


def header():
    return element(
        b"\x1a\x45\xdf\xa3",
        uint(b"\x42\x86", 1)
        + uint(b"\x42\xf7", 1)
        + uint(b"\x42\xf2", 4)
        + uint(b"\x42\xf3", 8)
        + element(b"\x42\x82", b"matroska")
        + uint(b"\x42\x87", 4)
        + uint(b"\x42\x85", 2),
    )


def track(number, kind, codec, *fields):
    return element(b"\xae", uint(b"\xd7", number) + uint(b"\x73\xc5", number) + uint(b"\x83", kind) + element(b"\x86", codec) + b"".join(fields))


def audio(rate, channels, bits):
    return element(b"\xe1", double(b"\xb5", rate) + uint(b"\x9f", channels) + uint(b"\x62\x64", bits))


def block(number, relative, flags, payload):
    return size(number) + struct.pack(">h", relative) + bytes([flags]) + payload


def xiph(frames):
    sizes = b"".join(b"\xff" * (len(frame) // 255) + bytes([len(frame) % 255]) for frame in frames[:-1])
    return bytes([len(frames) - 1]) + sizes + b"".join(frames)


def ebml_laced(frames):
    sizes = size(len(frames[0]))
    for previous, frame in zip(frames, frames[1:-1]):
        difference = len(frame) - len(previous)
        length = 1
        while not -(1 << (7 * length - 1)) + 1 <= difference <= (1 << (7 * length - 1)) - 1:
            length += 1
        biased = difference + (1 << (7 * length - 1)) - 1
        sizes += bytes([(1 << (8 - length)) | (biased >> (8 * (length - 1)))]) + biased.to_bytes(length, "big")[1:]
    return bytes([len(frames) - 1]) + sizes + b"".join(frames)


def tone(first, count, rate, channels, bits, big):
    scale = (1 << (bits - 1)) // 8
    samples = []
    for n in range(first, first + count):
        for channel in range(channels):
            value = round(scale * math.sin(2 * math.pi * (440 + 220 * channel) * n / rate))
            samples.append(value.to_bytes(bits // 8, "big" if big else "little", signed=True))
    return b"".join(samples)


def laced(opus_path, target):
    private, delay, packets = source(opus_path, 1)
    # Blocks as (time, track, element ID, flags, laced payload, BlockGroup elements after it).
    events = []
    for at in range(0, len(packets), 4):
        group = packets[at : at + 4]
        events.append((group[0][0], 1, b"\xa3", 0x80 | 0x02, xiph([frame for _, frame in group]), b""))
    # Three to a group, so the last group holds more than one frame (81 packets).
    for at in range(0, len(packets), 3):
        group = packets[at : at + 3]
        extra = uint(b"\x9b", 20 * len(group))
        if at + 3 >= len(packets):
            extra += sint(b"\x75\xa2", 20_000_000)
        events.append((group[0][0], 3, b"\xa0", 0x06, ebml_laced([frame for _, frame in group]), extra))
    for at in range(0, 1600, 60):
        frames = [tone(8 * (at + 20 * n), 160, 8_000, 1, 16, False) for n in range(3)]
        events.append((at, 2, b"\xa3", 0x80 | 0x04, bytes([2]) + b"".join(frames), b""))
    events.sort(key=lambda event: (event[0], event[1]))
    tracks = element(
        b"\x16\x54\xae\x6b",
        track(1, 2, b"A_OPUS", uint(b"\x56\xaa", delay), uint(b"\x56\xbb", 80_000_000), audio(48_000, 1, 16), element(b"\x63\xa2", private))
        + track(2, 2, b"A_PCM/INT/LIT", uint(b"\x23\xe3\x83", 20_000_000), uint(b"\x88", 0), element(b"\x22\xb5\x9c", b"fra"), element(b"\x53\x6e", "Tonalit\u00e9".encode()), audio(8_000, 1, 16))
        + track(3, 2, b"A_OPUS", uint(b"\x56\xaa", delay), uint(b"\x88", 0), element(b"\x22\xb5\x9c", b"und"), audio(48_000, 1, 16), element(b"\x63\xa2", private)),
    )
    info = element(b"\x15\x49\xa9\x66", uint(b"\x2a\xd7\xb1", 1_000_000) + double(b"\x44\x89", 1_620.0) + element(b"\x4d\x80", b"alpha2") + element(b"\x57\x41", b"alpha2"))
    seek_head_size = 3 * 21 + 5
    clusters = b""
    cues = []
    offset = seek_head_size + len(info) + len(tracks)
    for start in range(0, 1700, 500):
        inside = [event for event in events if start <= event[0] < start + 500]
        if not inside:
            continue
        cues.append((start, offset + len(clusters)))
        body = uint(b"\xe7", start)
        for time, number, ident, flags, payload, extra in inside:
            laid = block(number, time - start, flags, payload)
            if ident == b"\xa0":
                body += element(b"\xa0", element(b"\xa1", laid) + extra)
            else:
                body += element(b"\xa3", laid)
        clusters += element(b"\x1f\x43\xb6\x75", body)
    cue_points = b"".join(
        element(b"\xbb", uint(b"\xb3", time) + element(b"\xb7", uint(b"\xf7", 1) + uint(b"\xf1", position))) for time, position in cues
    )
    positions = [(b"\x15\x49\xa9\x66", seek_head_size), (b"\x16\x54\xae\x6b", seek_head_size + len(info)), (b"\x1c\x53\xbb\x6b", offset + len(clusters))]
    seeks = b"".join(element(b"\x4d\xbb", element(b"\x53\xab", ident) + element(b"\x53\xac", position.to_bytes(8, "big"))) for ident, position in positions)
    seek_head = element(b"\x11\x4d\x9b\x74", seeks)
    assert len(seek_head) == seek_head_size, (len(seek_head), seek_head_size)
    segment = seek_head + info + tracks + clusters + element(b"\x1c\x53\xbb\x6b", cue_points)
    with open(target, "wb") as file:
        file.write(header() + element(b"\x18\x53\x80\x67", segment))


def groups(mpeg4_path, target):
    private, _, frames = source(mpeg4_path, 2)
    frames = [frame for _, frame in frames]
    stripped = b"\x00\x00\x01"
    assert all(frame.startswith(stripped) for frame in frames)
    video = element(b"\xe0", uint(b"\xb0", 64) + uint(b"\xba", 48))
    encoding = element(
        b"\x6d\x80",
        element(b"\x62\x40", uint(b"\x50\x32", 1) + element(b"\x50\x34", uint(b"\x42\x54", 3) + element(b"\x42\x55", stripped))),
    )
    tracks = element(
        b"\x16\x54\xae\x6b",
        track(1, 1, b"V_MPEG4/ISO/ASP", uint(b"\x23\xe3\x83", 40_000_000), video, element(b"\x63\xa2", private))
        + track(2, 1, b"V_MPEG4/ISO/ASP", uint(b"\x23\xe3\x83", 40_000_000), uint(b"\x88", 0), encoding, video, element(b"\x63\xa2", private))
        + track(3, 2, b"A_PCM/INT/BIG", audio(11_025, 2, 24))
        + track(4, 2, b"V_MPEG4/ISO/ASP", audio(11_025, 2, 24))
        + track(5, 7, b"B_CONTROL"),
    )
    info = crc(uint(b"\x2a\xd7\xb1", 100_000) + double(b"\x44\x89", 12_000.0) + element(b"\x4d\x80", b"alpha2") + element(b"\x57\x41", b"alpha2"))
    segment = element(b"\x15\x49\xa9\x66", info) + tracks + element(b"\xec", bytes(7)) + element(b"\x7e\x7e", b"alpha2")
    sample = 0
    for gop in range(6):
        start = gop * len(frames) * 400
        body = uint(b"\xe7", start) + element(b"\xec", bytes(3))
        for index, frame in enumerate(frames):
            relative = index * 400
            group = element(b"\xa1", block(1, relative, 0, frame))
            if index:
                group += sint(b"\xfb", -400)
            if index % 4 == 3:
                group += uint(b"\x9b", 400)
            body += element(b"\xa0", group)
            body += element(b"\xa3", block(2, relative, 0x80 if index == 0 else 0, frame[len(stripped) :]))
            if index % 2 == 0:
                pcm = [tone(sample + 256 * n, 256, 11_025, 2, 24, True) for n in range(2)]
                body += element(b"\xa3", block(3, round(sample * 10_000 / 11_025) - start, 0x80 | 0x06, ebml_laced(pcm)))
                sample += 512
            body += element(b"\x7e\x7e", b"x")
        segment += b"\x1f\x43\xb6\x75" + UNKNOWN_SIZE + body
    with open(target, "wb") as file:
        file.write(header() + b"\x18\x53\x80\x67" + UNKNOWN_SIZE + segment)


laced(sys.argv[1], sys.argv[3])
groups(sys.argv[2], sys.argv[4])

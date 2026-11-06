#!/usr/bin/env python3
"""Records what FFmpeg reports for every MP4, MOV and M4A file of the corpus.

Usage: container-mp4.py <corpus directory> <expected.json>

Per track: stream facts, the packet count and a SHA-256 over each packet's
`pts,dts,duration,keyframe,size,sha256,skip,padding` line. Remuxable files also get FFmpeg's own
`-c copy -movflags +faststart` copy read back. A fragmented copy of `h264-bt709-copy.mp4` is
written beside `expected.json` and recorded under `fixtures`.
"""

import hashlib
import json
import math
import os
import struct
import subprocess
import sys
import tempfile

REMUXED_CODECS = {"h264", "hevc", "av1", "vp9", "mpeg4", "prores", "aac", "opus"}
SHOWN_PACKETS = 8
FRAGMENTED = "container-mp4-fragmented.mp4"


def boxes(data, start, end):
    at = start
    while at + 8 <= end:
        size, kind = struct.unpack_from(">I4s", data, at)
        header = 8
        if size == 1:
            size = struct.unpack_from(">Q", data, at + 8)[0]
            header = 16
        elif size == 0:
            size = end - at
        if size < header or at + size > end:
            return
        yield at, at + header, at + size, kind
        at += size


def child(data, start, end, kind):
    for _, body, stop, found in boxes(data, start, end):
        if found == kind:
            return body, stop
    return None


def entry_boxes(path):
    """The child box types of each track's first sample entry, by track id."""
    data = open(path, "rb").read()
    moov = child(data, 0, len(data), b"moov")
    found = {}
    if not moov:
        return found
    for _, body, stop, kind in boxes(data, *moov):
        if kind != b"trak":
            continue
        tkhd = child(data, body, stop, b"tkhd")
        version = data[tkhd[0]]
        track_id = struct.unpack_from(">I", data, tkhd[0] + (20 if version == 1 else 12))[0]
        place = (body, stop)
        for step in [b"mdia", b"minf", b"stbl", b"stsd"]:
            place = place and child(data, place[0], place[1], step)
        if not place:
            continue
        entries = list(boxes(data, place[0] + 8, place[1]))
        if not entries:
            continue
        _, entry_body, entry_end, _ = entries[0]
        hdlr = child(data, *child(data, body, stop, b"mdia"), b"hdlr")
        handler = data[hdlr[0] + 8 : hdlr[0] + 12]
        if handler == b"vide":
            skip = 78
        elif handler == b"soun":
            version = struct.unpack_from(">H", data, entry_body + 8)[0]
            skip = 28 + {1: 16, 2: 36}.get(version, 0)
        else:
            skip = len(data)
        kinds = set()
        for _, child_body, child_end, kind in boxes(data, entry_body + skip, entry_end):
            kinds.add(kind)
            if kind == b"wave":
                kinds |= {inner for _, _, _, inner in boxes(data, child_body, child_end)}
        found[track_id] = kinds
    return found


def probe(path, *extra):
    output = subprocess.run(
        ["ffprobe", "-v", "error", *extra, "-of", "json", path],
        capture_output=True,
        text=True,
        check=False,
    )
    return output.returncode, output.stdout, output.stderr.strip()


def orientation(side_data):
    """Clockwise quarter turns and mirror of a display matrix, as alpha2's FFmpeg host reads it."""
    for entry in side_data:
        if entry.get("side_data_type") != "Display Matrix":
            continue
        numbers = [int(word) for line in entry["displaymatrix"].strip().splitlines() for word in line.split()[1:]]
        flipped = numbers[0] * numbers[4] - numbers[1] * numbers[3] < 0
        if flipped:
            numbers[0], numbers[3], numbers[6] = -numbers[0], -numbers[3], -numbers[6]
        scale0 = math.hypot(numbers[0] / 65536, numbers[3] / 65536)
        scale1 = math.hypot(numbers[1] / 65536, numbers[4] / 65536)
        angle = -math.degrees(math.atan2(numbers[1] / 65536 / scale1, numbers[0] / 65536 / scale0))
        clockwise = (angle if flipped else -angle) % 360
        turns = round(clockwise / 90) % 4
        if abs(clockwise - round(clockwise / 90) * 90) >= 0.5:
            turns = 0
        return {"rotation": turns * 90, "mirror": flipped}
    return {"rotation": 0, "mirror": False}


def packet_line(packet):
    skip = padding = 0
    for side in packet.get("side_data_list", []):
        if side.get("side_data_type") == "Skip Samples":
            skip, padding = side["skip_samples"], side["discard_padding"]
    digest = packet["data_hash"].split(":", 1)[1]
    key = 1 if packet["flags"][0] == "K" else 0
    return [packet.get("pts"), packet.get("dts"), packet.get("duration"), key, int(packet["size"]), digest, skip, padding]


def packets_of(packets, index):
    lines = [packet_line(packet) for packet in packets if packet["stream_index"] == index]
    digest = hashlib.sha256()
    for line in lines:
        digest.update((",".join(str(value) for value in line) + "\n").encode())
    discarded = sum(1 for packet in packets if packet["stream_index"] == index and "D" in packet["flags"])
    return {
        "packet_count": len(lines),
        "packets_sha256": digest.hexdigest(),
        "discarded": discarded,
        "first_packets": lines[:SHOWN_PACKETS],
    }


def standard_layouts():
    """FFmpeg's named channel layouts and their channels."""
    output = subprocess.run(["ffmpeg", "-layouts"], capture_output=True, text=True, check=True).stdout
    table = output.split("DECOMPOSITION", 1)[1]
    return dict(line.split() for line in table.strip().splitlines() if len(line.split()) == 2)


LAYOUTS = standard_layouts()


def speakers(layout):
    """The channel names of a layout FFmpeg reports, or `ambisonic` for an ambisonic one."""
    if not layout:
        return None
    if layout.startswith("ambisonic"):
        return "ambisonic"
    if "(" in layout and layout.endswith(")") and "channels" in layout:
        return layout.split("(", 1)[1][:-1].split("+")
    return LAYOUTS[layout].split("+")


def facts(path):
    code, out, err = probe(path, "-show_streams", "-show_packets", "-show_data_hash", "SHA256")
    if code != 0:
        return {"error": err.splitlines()[-1] if err else f"ffprobe exit {code}"}
    report = json.loads(out)
    entries = entry_boxes(path)
    tracks = []
    for stream in report.get("streams", []):
        disposition = stream.get("disposition", {})
        tags = stream.get("tags", {})
        track_id = int(stream.get("id", "0"), 16)
        kinds = entries.get(track_id, set())
        track = {
            "index": stream["index"],
            "id": track_id,
            "kind": stream["codec_type"],
            "codec": stream.get("codec_name"),
            "codec_tag": stream.get("codec_tag_string"),
            "timescale": int(stream["time_base"].split("/")[1]),
            "duration": stream.get("duration_ts"),
            "default": bool(disposition.get("default")),
            "attached_pic": bool(disposition.get("attached_pic")),
            "language": tags.get("language"),
            "title": tags.get("title"),
            "handler": tags.get("handler_name"),
        }
        if stream["codec_type"] == "video":
            track["width"] = stream.get("width")
            track["height"] = stream.get("height")
            track["orientation"] = orientation(stream.get("side_data_list", []))
            # Without `pasp` or `colr`, FFmpeg reports what its decoder found in the bitstream.
            track["sar"] = stream.get("sample_aspect_ratio") if b"pasp" in kinds else None
            track["color"] = (
                {
                    "primaries": stream.get("color_primaries", "unknown"),
                    "transfer": stream.get("color_transfer", "unknown"),
                    "space": stream.get("color_space", "unknown"),
                    "range": stream.get("color_range", "unknown"),
                }
                if b"colr" in kinds
                else None
            )
        elif stream["codec_type"] == "audio":
            track["sample_rate"] = int(stream.get("sample_rate", 0))
            track["channels"] = stream.get("channels")
            track["channel_layout"] = stream.get("channel_layout")
            track["bits_per_sample"] = stream.get("bits_per_sample")
            track["speakers"] = (
                speakers(stream.get("channel_layout"))
                if kinds & {b"chan", b"chnl", b"dac3", b"dec3"}
                else None
            )
        track.update(packets_of(report.get("packets", []), stream["index"]))
        tracks.append(track)
    return {"tracks": tracks}


def remux(path, scratch):
    code, out, _ = probe(path, "-show_streams")
    if code != 0:
        return None
    streams = json.loads(out).get("streams", [])
    if not streams or any(
        stream.get("codec_name") not in REMUXED_CODECS or stream.get("disposition", {}).get("attached_pic")
        for stream in streams
    ):
        return None
    target = os.path.join(scratch, "remux" + (".mov" if path.endswith(".mov") else ".mp4"))
    subprocess.run(
        ["ffmpeg", "-v", "fatal", "-y", "-i", path, "-map", "0", "-c", "copy", "-movflags", "+faststart", target],
        check=True,
    )
    return facts(target)


def fragmented(corpus, cases):
    """Writes a fragmented copy of a corpus file under the cases directory and reads it."""
    target = os.path.join(cases, FRAGMENTED)
    subprocess.run(
        [
            "ffmpeg", "-v", "fatal", "-y", "-fflags", "+bitexact", "-i", os.path.join(corpus, "h264-bt709-copy.mp4"),
            "-map", "0", "-c", "copy", "-movflags", "frag_keyframe+empty_moov+default_base_moof", target,
        ],
        check=True,
    )
    return facts(target)


def main():
    corpus, expected = sys.argv[1], sys.argv[2]
    names = sorted(name for name in os.listdir(corpus) if name.endswith((".mp4", ".mov", ".m4a")))
    version = subprocess.run(["ffprobe", "-version"], capture_output=True, text=True, check=True).stdout.split()[2]
    files = {}
    with tempfile.TemporaryDirectory() as scratch:
        for name in names:
            path = os.path.join(corpus, name)
            files[name] = facts(path)
            remuxed = remux(path, scratch)
            if remuxed:
                files[name]["remux"] = remuxed
    fixtures = {FRAGMENTED: fragmented(corpus, os.path.dirname(os.path.abspath(expected)))}
    with open(expected, "w") as output:
        json.dump({"ffprobe": version, "files": files, "fixtures": fixtures}, output, indent=1, sort_keys=True)
        output.write("\n")


main()

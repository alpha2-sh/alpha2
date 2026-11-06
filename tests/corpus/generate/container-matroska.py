#!/usr/bin/env python3
"""Records what FFmpeg reports for every Matroska and WebM file of the corpus.

Usage: container-matroska.py <corpus directory> <ffprobe> <expected.json>

Per track: stream facts, the packet count and a SHA-256 over each packet's
`pts,dts,duration,keyframe,size,sha256,skip,padding` line. A duration FFmpeg leaves unset is 0. A
decode time it leaves unset (the first packets of a reordered stream) is the next packet's decode
time less the packet's duration (at least 1), as alpha2-container fills it, or the presentation
time when no later packet has one.

Files whose codecs alpha2's Matroska muxer writes also get FFmpeg's own `-c copy -copyts` read
back under `remux`: the muxer gets the timestamps the demuxer read, as alpha2's muxer does. Stream copies of other corpus files, for codecs and layouts the Matroska files lack, are
written beside `expected.json` and recorded under `fixtures`.
"""

import hashlib
import json
import os
import struct
import subprocess
import sys
import tempfile

SHOWN_PACKETS = 8
REMUXED_CODECS = {
    "h264", "hevc", "vp8", "vp9", "av1", "mpeg4", "aac", "opus", "vorbis", "flac", "mp3", "ac3", "eac3",
    "pcm_s16le", "pcm_s24le", "pcm_s32le", "pcm_s16be", "pcm_f32le", "pcm_u8",
}
# Stream copies of corpus files into Matroska: source, fixture written under the cases directory,
# and extra output options.
COPIES = [
    ("hevc-topleft.mp4", "container-matroska-hevc.mkv"),
    ("h264-long-gop.mp4", "container-matroska-h264-long-gop.mkv"),
    ("long-gop.mp4", "container-matroska-mpeg4-b-frames.mkv"),
    ("av1.mp4", "container-matroska-av1.webm"),
    ("sar-16-15.mp4", "container-matroska-sar.mkv"),
    ("vp9.webm", "container-matroska-aspect.webm", "-aspect", "16:9"),
    ("rotation.mp4", "container-matroska-rotation.mkv"),
    ("h264-bt709-hflip.mp4", "container-matroska-hflip.mkv"),
    ("hdr-pq.mp4", "container-matroska-hdr-pq.mkv"),
    ("audio-vorbis-5.1.ogg", "container-matroska-vorbis.webm"),
    ("audio-mp3-lame.mp3", "container-matroska-mp3.mkv"),
    ("audio-ac3-5.1.mp4", "container-matroska-ac3.mkv"),
    ("audio-eac3-5.0.mp4", "container-matroska-eac3.mkv"),
    ("audio-f32-list.wav", "container-matroska-f32.mkv"),
    ("audio-u8-odd.wav", "container-matroska-u8.mkv"),
    ("audio-two-streams.mov", "container-matroska-two-streams.mkv"),
]

ID_EBML, ID_SEGMENT, ID_TRACKS, ID_TRACK_ENTRY = 0x1A45DFA3, 0x18538067, 0x1654AE6B, 0xAE
ID_TRACK_NUMBER, ID_TRACK_TYPE, ID_CODEC_ID, ID_CODEC_PRIVATE = 0xD7, 0x83, 0x86, 0x63A2
ID_VIDEO, ID_DISPLAY_UNIT, ID_COLOUR, ID_CLUSTER = 0xE0, 0x54B2, 0x55B0, 0x1F43B675
TRACK_PREFIXES = {1: "V", 2: "A", 17: "DS", 33: "DS"}


def vint(data, at, keep_marker):
    first = data[at]
    length = 1
    while length <= 8 and not first & (0x80 >> (length - 1)):
        length += 1
    value = first if keep_marker else first & (0xFF >> length)
    for byte in data[at + 1 : at + length]:
        value = (value << 8) | byte
    unknown = not keep_marker and value == (1 << (7 * length)) - 1
    return value, length, unknown


def elements(data, start, end):
    """The children of a master element: id, payload start, payload end."""
    at = start
    while at < end:
        ident, id_length, _ = vint(data, at, True)
        size, size_length, unknown = vint(data, at + id_length, False)
        body = at + id_length + size_length
        stop = end if unknown else min(body + size, end)
        yield ident, body, stop
        if ident == ID_CLUSTER:
            return
        at = stop


def child(data, start, end, ident):
    return next(((body, stop) for found, body, stop in elements(data, start, end) if found == ident), None)


def uint(data, place):
    return int.from_bytes(data[place[0] : place[1]], "big") if place else None


def track_entries(path):
    """The track entries FFmpeg turns into streams, in order, with what the expectations need."""
    data = open(path, "rb").read()
    segment = child(data, 0, len(data), ID_SEGMENT)
    tracks = segment and child(data, *segment, ID_TRACKS)
    found = []
    for ident, body, stop in elements(data, *tracks) if tracks else []:
        if ident != ID_TRACK_ENTRY:
            continue
        kind = uint(data, child(data, body, stop, ID_TRACK_TYPE))
        codec = child(data, body, stop, ID_CODEC_ID)
        codec = data[codec[0] : codec[1]].rstrip(b"\0").decode() if codec else ""
        if kind not in TRACK_PREFIXES or not codec or codec[0] not in TRACK_PREFIXES[kind]:
            continue
        video = child(data, body, stop, ID_VIDEO)
        unit = uint(data, child(data, *video, ID_DISPLAY_UNIT)) if video else None
        private = child(data, body, stop, ID_CODEC_PRIVATE)
        found.append(
            {
                "number": uint(data, child(data, body, stop, ID_TRACK_NUMBER)),
                "codec_id": codec,
                "display_unit": 0 if unit is None else unit,
                "colour": bool(video and child(data, *video, ID_COLOUR)),
                "private": data[private[0] : private[1]] if private else b"",
            }
        )
    return found


def probe(ffprobe, path, *extra):
    output = subprocess.run([ffprobe, "-v", "error", *extra, "-of", "json", path], capture_output=True, text=True, check=False)
    return output.returncode, output.stdout, output.stderr.strip()


def orientation(side_data):
    """Clockwise quarter turns and mirror of a display matrix, as alpha2's FFmpeg host reads it."""
    import math

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


def packet_lines(packets):
    """Packet lines with FFmpeg's unset decode times and durations filled as the docstring says."""
    lines = []
    for packet in packets:
        skip = padding = 0
        for side in packet.get("side_data_list", []):
            if side.get("side_data_type") == "Skip Samples":
                skip, padding = side["skip_samples"], side["discard_padding"]
        digest = packet["data_hash"].split(":", 1)[1]
        key = 1 if packet["flags"][0] == "K" else 0
        lines.append([packet.get("pts"), packet.get("dts"), packet.get("duration") or 0, key, int(packet["size"]), digest, skip, padding])
    for at in range(len(lines) - 2, -1, -1):
        if lines[at][1] is None and lines[at + 1][1] is not None:
            lines[at][1] = lines[at + 1][1] - max(lines[at][2], 1)
    for line in lines:
        if line[1] is None:
            line[1] = line[0]
    return lines


def packets_of(packets, index):
    lines = packet_lines([packet for packet in packets if packet["stream_index"] == index])
    digest = hashlib.sha256()
    for line in lines:
        digest.update((",".join(str(value) for value in line) + "\n").encode())
    return {"packet_count": len(lines), "packets_sha256": digest.hexdigest(), "first_packets": lines[:SHOWN_PACKETS]}


def standard_layouts(ffprobe):
    ffmpeg = os.path.join(os.path.dirname(ffprobe), "ffmpeg") if os.path.dirname(ffprobe) else "ffmpeg"
    output = subprocess.run([ffmpeg, "-layouts"], capture_output=True, text=True, check=True).stdout
    table = output.split("DECOMPOSITION", 1)[1]
    return dict(line.split() for line in table.strip().splitlines() if len(line.split()) == 2)


def speakers(layout, layouts):
    if not layout:
        return None
    if "(" in layout and layout.endswith(")") and "channels" in layout:
        return layout.split("(", 1)[1][:-1].split("+")
    return layouts[layout].split("+")


def facts(ffprobe, layouts, path):
    code, out, err = probe(ffprobe, path, "-show_streams", "-show_packets", "-show_data_hash", "SHA256")
    if code != 0:
        return {"error": err.splitlines()[-1] if err else f"ffprobe exit {code}"}
    report = json.loads(out)
    entries = track_entries(path)
    streams = [stream for stream in report.get("streams", []) if stream["codec_type"] != "attachment"]
    tracks = []
    for stream, entry in zip(streams, entries):
        disposition = stream.get("disposition", {})
        tags = stream.get("tags", {})
        time_base = stream["time_base"].split("/")
        track = {
            "index": stream["index"],
            "id": entry["number"],
            "kind": stream["codec_type"],
            "codec": stream.get("codec_name"),
            "codec_id": entry["codec_id"],
            "time_base": stream["time_base"],
            "timescale": int(time_base[1]),
            "duration": stream.get("duration_ts"),
            "default": bool(disposition.get("default")),
            "language": tags.get("language"),
            "title": tags.get("title"),
        }
        if stream["codec_type"] == "video":
            track["width"] = stream.get("width")
            track["height"] = stream.get("height")
            track["orientation"] = orientation(stream.get("side_data_list", []))
            # FFmpeg derives the stream's ratio from the display size unless the unit is unknown.
            track["sar"] = stream.get("sample_aspect_ratio") if entry["display_unit"] < 4 else None
            track["color"] = (
                {
                    "primaries": stream.get("color_primaries", "unknown"),
                    "transfer": stream.get("color_transfer", "unknown"),
                    "space": stream.get("color_space", "unknown"),
                    "range": stream.get("color_range", "unknown"),
                }
                if entry["colour"]
                else None
            )
        elif stream["codec_type"] == "audio":
            track["sample_rate"] = int(stream.get("sample_rate", 0))
            track["channels"] = stream.get("channels")
            track["channel_layout"] = stream.get("channel_layout")
            # FFmpeg reports PCM bits per sample, and a FLAC STREAMINFO's as raw bits.
            bits = stream.get("bits_per_sample") or 0
            if not bits and stream.get("codec_name") == "flac":
                bits = int(stream.get("bits_per_raw_sample", 0) or 0)
            track["bits_per_sample"] = bits or None
            track["initial_padding"] = stream.get("initial_padding", 0)
            # The codec headers of these declare a layout; the decoders of the rest pick one.
            declared = stream.get("codec_name") in ("flac", "opus", "vorbis")
            track["speakers"] = speakers(stream.get("channel_layout"), layouts) if declared else None
        track.update(packets_of(report.get("packets", []), stream["index"]))
        tracks.append(track)
    return {"tracks": tracks}


def remux(ffprobe, layouts, path, scratch):
    code, out, _ = probe(ffprobe, path, "-show_streams", "-show_packets")
    if code != 0:
        return None
    report = json.loads(out)
    streams = report.get("streams", [])
    if not streams or any(stream.get("codec_name") not in REMUXED_CODECS for stream in streams):
        return None
    # FFmpeg's stream copy mangles timestamps this far out; the files test their refusal.
    if any(abs(packet.get("pts") or 0) > 1 << 53 for packet in report.get("packets", [])):
        return None
    ffmpeg = os.path.join(os.path.dirname(ffprobe), "ffmpeg")
    target = os.path.join(scratch, "remux" + os.path.splitext(path)[1])
    subprocess.run([ffmpeg, "-v", "fatal", "-y", "-copyts", "-i", path, "-map", "0", "-c", "copy", target], check=True)
    return facts(ffprobe, layouts, target)


def main():
    corpus, ffprobe, expected = sys.argv[1], sys.argv[2], sys.argv[3]
    cases = os.path.dirname(os.path.abspath(expected))
    ffmpeg = os.path.join(os.path.dirname(ffprobe), "ffmpeg")
    layouts = standard_layouts(ffprobe)
    names = sorted(name for name in os.listdir(corpus) if name.endswith((".mkv", ".webm")))
    version = subprocess.run([ffprobe, "-version"], capture_output=True, text=True, check=True).stdout.split()[2]
    files = {}
    with tempfile.TemporaryDirectory() as scratch:
        for name in names:
            path = os.path.join(corpus, name)
            files[name] = facts(ffprobe, layouts, path)
            remuxed = remux(ffprobe, layouts, path, scratch)
            if remuxed:
                files[name]["remux"] = remuxed
        fixtures = {}
        for source, name, *options in COPIES:
            target = os.path.join(cases, name)
            subprocess.run(
                [ffmpeg, "-v", "fatal", "-y", "-i", os.path.join(corpus, source), "-map", "0", "-c", "copy", "-fflags", "+bitexact", *options, target],
                check=True,
            )
            fixtures[name] = facts(ffprobe, layouts, target)
            remuxed = remux(ffprobe, layouts, target, scratch)
            if remuxed:
                fixtures[name]["remux"] = remuxed
    with open(expected, "w") as output:
        json.dump({"ffprobe": version, "files": files, "fixtures": fixtures}, output, indent=1, sort_keys=True)
        output.write("\n")


main()

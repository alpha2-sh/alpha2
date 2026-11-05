#!/usr/bin/env python3
"""Records what FFmpeg's demuxers report for the audio-only corpus files.

Usage: container-audio.py <corpus directory> <ffprobe> <output.json>

alpha2-container's audio readers are tested against the output. ADTS packets are recorded
without their ADTS header and CRC, as the reader hands out raw AAC access units.
"""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

corpus = Path(sys.argv[1])
ffprobe = sys.argv[2]
ffmpeg = os.path.join(os.path.dirname(ffprobe), "ffmpeg") if os.path.dirname(ffprobe) else "ffmpeg"
EXTENSIONS = {".wav", ".aac", ".flac", ".ogg", ".mp3"}
SPELLED = 8


def probe(path, *args):
    return json.loads(subprocess.check_output([ffprobe, "-v", "error", *args, "-show_data_hash", "sha256", "-of", "json", str(path)]))


def layouts():
    names = {}
    listing = subprocess.check_output([ffmpeg, "-hide_banner", "-layouts"], text=True)
    for line in listing.split("Standard channel layouts:")[1].splitlines()[2:]:
        parts = line.split()
        if len(parts) == 2:
            names[parts[0]] = parts[1].split("+")
    return names


def speakers(name, names):
    if name in (None, "unknown") or "channels" in name:
        return None
    return names.get(name, name.split("+"))


def tag(tags, key):
    return next((value for name, value in tags.items() if name.lower() == key), None)


def digest(text):
    return text.split(":", 1)[1].lower() if text else None


names = layouts()
files = {}
for path in sorted(corpus.iterdir()):
    if path.suffix not in EXTENSIONS:
        continue
    stream = probe(path, "-show_streams")["streams"][0]
    raw = path.read_bytes()
    rows = []
    for packet in probe(path, "-show_packets")["packets"]:
        size, data_hash = int(packet["size"]), digest(packet.get("data_hash"))
        if path.suffix == ".aac":
            pos = int(packet["pos"])
            header = 7 if raw[pos + 1] & 1 else 9
            payload = raw[pos + header : pos + size]
            size, data_hash = len(payload), hashlib.sha256(payload).hexdigest()
        skip = next((s for s in packet.get("side_data_list", []) if s.get("side_data_type") == "Skip Samples"), {})
        rows.append([
            int(packet["pts"]),
            int(packet["dts"]),
            int(packet.get("duration", 0)),
            "K" in packet["flags"],
            size,
            data_hash,
            int(skip.get("skip_samples", 0)),
            int(skip.get("discard_padding", 0)),
        ])
    stream_hash = hashlib.sha256()
    for pts, dts, duration, key, size, data_hash, skip, discard in rows:
        stream_hash.update(f"{pts} {dts} {duration} {int(key)} {size} {data_hash} {skip} {discard}\n".encode())
    num, den = (int(part) for part in stream["time_base"].split("/"))
    assert num == 1, path
    bits = int(stream.get("bits_per_sample") or 0) or int(stream.get("bits_per_raw_sample", "0").replace("N/A", "0"))
    tags = stream.get("tags", {})
    duration = stream.get("duration_ts")
    files[path.name] = {
        "codec": stream["codec_name"],
        "sample_rate": int(stream["sample_rate"]),
        "channels": int(stream["channels"]),
        "layout": speakers(stream.get("channel_layout"), names),
        "timescale": den,
        "duration": int(duration) if duration is not None else None,
        "bits_per_sample": bits or None,
        "initial_padding": int(stream.get("initial_padding", 0)),
        "extradata_sha256": digest(stream.get("extradata_hash")),
        "default": bool(stream["disposition"]["default"]),
        "title": tag(tags, "title"),
        "language": tag(tags, "language"),
        "packet_count": len(rows),
        "packets_sha256": stream_hash.hexdigest(),
        "packets": rows[:SPELLED],
    }

comment = (
    "Written by tests/corpus/generate/container-audio.py from ffprobe. Packets are "
    "[pts, dts, duration, keyframe, size, sha256 of data, skip samples, discard padding]; "
    "packets_sha256 hashes every packet's fields joined by spaces, one line each. ADTS packets "
    "exclude their ADTS header and CRC."
)
output = {"$comment": comment, "files": files}
Path(sys.argv[3]).write_text(json.dumps(output, indent=1) + "\n")

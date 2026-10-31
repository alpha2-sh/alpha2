#!/usr/bin/env bash
# The FFmpeg-made media: `generate.sh media` runs it with the corpus directory to write into.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "${1:?usage: media.sh <corpus directory>}"

# `-fflags +bitexact` here is an input option. Matroska/WebM outputs and each encoder need their
# own, or the file embeds a random UID or FFmpeg's version and cannot be regenerated to its hash.
FFMPEG=(ffmpeg -hide_banner -loglevel error -y -fflags +bitexact)

"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=30000/1001:duration=0.4 \
  -an -c:v mpeg4 -pix_fmt yuv420p -g 12 -bf 2 -movflags +write_colr \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv cfr-bt709-limited.mp4

"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=30:duration=0.5 \
  -vf "select='eq(n,0)+eq(n,1)+eq(n,3)+eq(n,6)+eq(n,10)'" \
  -fps_mode vfr -an -c:v mpeg4 -pix_fmt yuv420p vfr.mp4

"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=30:duration=1.1 \
  -an -c:v mpeg4 -pix_fmt yuv420p -g 30 -bf 2 long-gop.mp4

# Frame times off the tick grid and rounded by the container: 30 intra frames whose luma is
# 16 + 6 * their index, at 90 fps in a 1/90000 time base (frame 2 at 2666.67 ticks) and at 29.97 fps
# in Matroska's milliseconds (frame 2 at 67 ms, 8040 ticks).
"${FFMPEG[@]}" -f lavfi -i "nullsrc=s=64x48:r=90,format=yuv420p,geq=lum='16+6*N':cb=128:cr=128" \
  -frames:v 30 -an -c:v mpeg4 -q:v 1 -g 1 -video_track_timescale 90000 frame-index-90fps.mov
"${FFMPEG[@]}" -f lavfi -i "nullsrc=s=64x48:r=30000/1001,format=yuv420p,geq=lum='16+6*N':cb=128:cr=128" \
  -frames:v 30 -an -c:v mpeg4 -q:v 1 -g 1 -fflags +bitexact frame-index-2997.mkv

"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=24:duration=0.4 \
  -an -c:v mpeg4 -pix_fmt yuv420p .rotation-base.mp4
"${FFMPEG[@]}" -display_rotation:v:0 90 -i .rotation-base.mp4 -c copy rotation.mp4
rm -f .rotation-base.mp4
# FFmpeg applies the display matrix on decode (autorotate), which makes this the picture a player
# shows.
"${FFMPEG[@]}" -i rotation.mp4 -frames:v 1 -pix_fmt rgb24 -bitexact rotation-autorotate.png

# Still orientation: one upright picture stored eight ways, each tagged with the Exif Orientation
# that shows it upright again, so all eight display as `exif-orientation-1.jpg` stores it. 4:4:4 at
# a multiple of the 8x8 block on both axes, so each stored transform codes alike.
for entry in 1: 2:hflip 3:hflip,vflip 4:vflip 5:transpose=cclock_flip 6:transpose=cclock 7:transpose=clock_flip 8:transpose=clock; do
  orientation="${entry%%:*}"
  filter="${entry#*:}"
  "${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=1 -frames:v 1 -vf "${filter:+$filter,}format=yuvj444p" \
    -q:v 2 -flags:v +bitexact ".exif-${orientation}.jpg"
done
# A still whose embedded ICC profile is Display P3 (D50-adapted colorants, gamma 2.2): read as
# stored, sRGB code values, by both hosts.
"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=1 -frames:v 1 -vf format=yuvj444p \
  -q:v 2 -flags:v +bitexact .icc.jpg
python3 - <<'TAG'
import struct

def segment(marker, payload):
    return b"\xff" + bytes([marker]) + struct.pack(">H", len(payload) + 2) + payload

def tagged(source, target, *segments):
    data = open(source, "rb").read()
    assert data[:2] == b"\xff\xd8"
    open(target, "wb").write(data[:2] + b"".join(segments) + data[2:])

# One IFD entry: Orientation (0x0112), SHORT, count 1, little-endian TIFF.
for orientation in range(1, 9):
    tiff = b"II*\x00" + struct.pack("<IHHHIHHI", 8, 1, 0x0112, 3, 1, orientation, 0, 0)
    tagged(f".exif-{orientation}.jpg", f"exif-orientation-{orientation}.jpg", segment(0xE1, b"Exif\x00\x00" + tiff))

def s15f16(value):
    return struct.pack(">i", round(value * 65536))

def xyz(x, y, z):
    return b"XYZ \x00\x00\x00\x00" + s15f16(x) + s15f16(y) + s15f16(z)

def text(value):
    return b"desc\x00\x00\x00\x00" + struct.pack(">I", len(value) + 1) + value + b"\x00" + b"\x00" * 79

curve = b"curv\x00\x00\x00\x00" + struct.pack(">IH", 1, 0x0233) + b"\x00\x00"
tags = [
    (b"desc", text(b"Display P3 (gamma 2.2)")),
    (b"cprt", b"text\x00\x00\x00\x00" + b"CC0\x00"),
    (b"wtpt", xyz(0.9642, 1.0, 0.8249)),
    (b"rXYZ", xyz(0.515121, 0.241196, -0.001053)),
    (b"gXYZ", xyz(0.291977, 0.692245, 0.041885)),
    (b"bXYZ", xyz(0.157104, 0.066574, 0.784073)),
    (b"rTRC", curve),
    (b"gTRC", curve),
    (b"bTRC", curve),
]
offset = 128 + 4 + 12 * len(tags)
table = b""
body = b""
for signature, data in tags:
    data += b"\x00" * (-len(data) % 4)
    table += signature + struct.pack(">II", offset + len(body), len(data))
    body += data
size = offset + len(body)
header = struct.pack(">I", size) + b"\x00\x00\x00\x00" + b"\x02\x10\x00\x00" + b"mntrRGB XYZ "
header += struct.pack(">6H", 2026, 1, 1, 0, 0, 0) + b"acsp" + b"\x00" * 24
header += struct.pack(">I", 0) + s15f16(0.9642) + s15f16(1.0) + s15f16(0.8249)
header += b"\x00" * (128 - len(header))
profile = header + struct.pack(">I", len(tags)) + table + body
assert len(profile) == size
tagged(".icc.jpg", "icc-display-p3.jpg", segment(0xE2, b"ICC_PROFILE\x00\x01\x01" + profile))
TAG
rm -f .exif-?.jpg .icc.jpg

"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=25:duration=0.5 \
  -itsoffset 0.125 -f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.375 \
  -map 0:v -map 1:a -c:v mpeg4 -pix_fmt yuv420p -c:a aac -shortest edit-list.mp4

# MPEG-TS starts its streams at the 1.4 s mux delay, not at zero.
"${FFMPEG[@]}" -f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.4 \
  -c:a aac -b:a 96k -f mpegts audio-mpegts.ts

# A broadcast capture's clock hours in: stereo AAC in MPEG-TS whose stream starts at 12 345 s, so a
# WAV from the source's time zero would hold 4.7 GB of silence first.
"${FFMPEG[@]}" -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=0.2" -ac 2 \
  -c:a aac -b:a 96k -flags:a +bitexact -output_ts_offset 12345 -f mpegts audio-late-start.ts

# A raw elementary stream: its demuxer seeks only through the index it builds while reading, so it
# cannot seek before the first packet.
"${FFMPEG[@]}" -f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.4 \
  -c:a aac -b:a 96k -flags:a +bitexact -f adts audio-adts.aac

# A raw ADTS stream whose rate changes: a 44.1 kHz file and a 48 kHz one, one after the other. Each
# frame's header names its rate; FFmpeg's demuxer times every packet at the first.
"${FFMPEG[@]}" -f lavfi -i sine=frequency=440:sample_rate=44100:duration=0.5 \
  -c:a aac -b:a 96k -flags:a +bitexact -f adts .rate-change-a.aac
"${FFMPEG[@]}" -f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.5 \
  -c:a aac -b:a 96k -flags:a +bitexact -f adts .rate-change-b.aac
cat .rate-change-a.aac .rate-change-b.aac > audio-adts-rate-change.aac
rm -f .rate-change-a.aac .rate-change-b.aac

"${FFMPEG[@]}" -f lavfi -i sine=frequency=997:sample_rate=44100:duration=0.4 \
  -vn -c:a aac -b:a 96k audio-only-aac-priming.m4a

# Clean audio timestamp gaps: a 440 Hz tone whose frames starting in [1 s, 2 s) are dropped before
# encoding, so the packets jump over a second of silence with nothing damaged. PCM in Matroska
# (20 ms frames, on its millisecond grid) and AAC in MPEG-TS.
"${FFMPEG[@]}" -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=3,asetnsamples=n=960:p=0,aselect='not(between(t,1,1.999))'" \
  -c:a pcm_s16le -fflags +bitexact -flags:a +bitexact audio-gap.mkv
"${FFMPEG[@]}" -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=3,asetnsamples=n=1024:p=0,aselect='not(between(t,1,1.999))'" \
  -c:a aac -b:a 96k -flags:a +bitexact -f mpegts audio-gap.ts

# Audio timestamps no 64-bit sample index holds: a jump 3e17 ms ahead, and a first cluster 3e17 ms
# before zero.
python3 "$here/audio-pts.py" audio-pts-ahead.mkv audio-pts-behind.mkv

# First-order ambisonics: four PCM channels a QuickTime `SA3D` box (inserted after the sample
# entry's own children) declares as ACN/SN3D order 1, which FFmpeg reads as the `ambisonic 1`
# layout.
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=0.4|0.3|0.2|0.1:c=4.0:s=48000:d=0.1" \
  -c:a pcm_s16le -fflags +bitexact -flags:a +bitexact .ambisonic.mov
python3 - <<'PATCH'
import struct
data = bytearray(open(".ambisonic.mov", "rb").read())
assert data.find(b"mdat") < data.find(b"moov")
path, start, end = [], 0, len(data)
for name in [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"sowt"]:
    at = start
    while data[at + 4 : at + 8] != name:
        at += struct.unpack_from(">I", data, at)[0]
        assert at < end, name
    path.append(at)
    start, end = at + (16 if name == b"stsd" else 8), at + struct.unpack_from(">I", data, at)[0]
payload = struct.pack(">BBIBBI", 0, 0, 1, 0, 0, 4) + b"".join(struct.pack(">I", channel) for channel in range(4))
box = struct.pack(">I4s", 8 + len(payload), b"SA3D") + payload
data[end:end] = box
for at in path:
    struct.pack_into(">I", data, at, struct.unpack_from(">I", data, at)[0] + len(box))
open("audio-ambisonic.mov", "wb").write(data)
PATCH
rm -f .ambisonic.mov

# Stream selection: a subtitle stream ahead of the video and the audio; an audio file whose only
# video stream is its cover art (an attached picture); and a video whose audio track is AC-4, which
# no decoder in the SDK reads (the `mp4a` sample entry renamed `ac-4` and its `esds` hidden).
printf '1\n00:00:00,000 --> 00:00:00,200\nalpha2\n' >.subtitle.srt
"${FFMPEG[@]}" -i .subtitle.srt -f lavfi -i testsrc2=size=64x48:rate=25:duration=0.2 \
  -f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.2 \
  -map 0:s -map 1:v -map 2:a -c:s srt -c:v mpeg4 -c:a aac -b:a 64k \
  -fflags +bitexact -flags:v +bitexact -flags:a +bitexact subtitle-first.mkv
rm -f .subtitle.srt
"${FFMPEG[@]}" -f lavfi -i sine=frequency=440:sample_rate=44100:duration=0.2 \
  -f lavfi -i color=red:size=64x48:rate=1 -map 0:a -map 1:v -frames:v 1 \
  -c:a aac -b:a 64k -c:v png -disposition:v attached_pic \
  -fflags +bitexact -flags:a +bitexact -flags:v +bitexact cover-art.m4a
"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=25:duration=0.2 \
  -f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.2 \
  -c:v mpeg4 -c:a aac -b:a 64k -fflags +bitexact -flags:v +bitexact -flags:a +bitexact .undecodable.mp4
python3 - <<'PATCH'
data = bytearray(open(".undecodable.mp4", "rb").read())
entry = data.find(b"mp4a")
assert entry > 0 and data.find(b"mp4a", entry + 1) == -1
descriptor = data.find(b"esds", entry)
data[entry : entry + 4] = b"ac-4"
data[descriptor : descriptor + 4] = b"skip"
open("audio-undecodable.mp4", "wb").write(data)
PATCH
rm -f .undecodable.mp4

# A variable frame rate that rises after FFmpeg has guessed it: 6 s at 30 fps, then 1 s at 60 fps,
# joined by the concat demuxer, so the stream's guessed rate is 30 and the last second has two
# frames to each of its slots.
"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=30:duration=6 -an -c:v mpeg4 \
  -enc_time_base 1/60 -g 30 -bf 0 -flags:v +bitexact .vfr-30.mp4
"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=60:duration=1 -an -c:v mpeg4 \
  -enc_time_base 1/60 -g 30 -bf 0 -flags:v +bitexact .vfr-60.mp4
printf 'file .vfr-30.mp4\nfile .vfr-60.mp4\n' >.vfr.txt
"${FFMPEG[@]}" -f concat -safe 0 -i .vfr.txt -c copy -fflags +bitexact vfr-rising.mp4
rm -f .vfr-30.mp4 .vfr-60.mp4 .vfr.txt

# One video frame and ten seconds of audio after it, in Matroska: a video decoder reads the audio's
# packets to reach the end.
"${FFMPEG[@]}" -f lavfi -i color=red:size=64x48:rate=25:duration=0.04 -f lavfi -i anullsrc=r=48000:cl=mono \
  -t 10 -c:v mpeg4 -c:a aac -b:a 16k -fflags +bitexact -flags:v +bitexact -flags:a +bitexact video-short-audio-long.mkv

# Opus, whose decoder asks for 80 ms of pre-roll after a seek: two tones, in MP4 for sample-exact
# timestamps (Matroska rounds them to the millisecond).
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=0.3*sin(2*PI*440*t)+0.2*sin(2*PI*3000*t):s=48000:d=0.6" \
  -c:a opus -strict experimental -b:a 96k -fflags +bitexact -flags:a +bitexact audio-opus.mp4

# Audio seek against a decode straight through: three tones, mono, one frame length. Matroska and
# WebM round frame timestamps to the millisecond; MP4 keeps the sample rate.
SEEK_TONES="aevalsrc=0.25*sin(2*PI*440*t)+0.2*sin(2*PI*1234.5*t)+0.15*sin(2*PI*3100*t):d=1.6"
for rate in 48000 44100; do
  suffix=$([[ $rate == 48000 ]] && echo "" || echo "-$rate")
  "${FFMPEG[@]}" -f lavfi -i "$SEEK_TONES:s=$rate" -c:a aac -b:a 96k -fflags +bitexact \
    -flags:a +bitexact "audio-seek-aac$suffix.mkv"
done
"${FFMPEG[@]}" -f lavfi -i "$SEEK_TONES:s=48000" -c:a aac -b:a 96k -fflags +bitexact -flags:a +bitexact \
  audio-seek-aac.mp4
"${FFMPEG[@]}" -i audio-seek-aac.mkv -c copy -fflags +bitexact audio-seek-aac-remux.mp4
for container in webm mp4; do
  "${FFMPEG[@]}" -f lavfi -i "$SEEK_TONES:s=48000" -c:a opus -strict experimental -b:a 96k \
    -fflags +bitexact -flags:a +bitexact "audio-seek-opus.$container"
done

"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=24:duration=0.4 \
  -an -c:v mpeg4 -pix_fmt yuv420p video-only.mp4

# 5.1 PCM at fixed speaker levels with a 50 ms offset, in QuickTime to keep the 1/48000 sample grid.
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=0.1|0.2|0.3|0.4|0.5|0.6:c=5.1:s=48000:d=0.1" \
  -c:a pcm_s16le -output_ts_offset 0.05 -fflags +bitexact -flags:a +bitexact \
  audio-surround-offset.mov

# Speaker layouts a source declares, each speaker at its own level: 3.0 and quad in a
# WAVE_FORMAT_EXTENSIBLE channel mask, C L R (a custom order) in QuickTime `chan` channel
# descriptions, quad as a `chan` layout tag, and 5.1.4 (ten channels, past a 7.1) in a mask.
for entry in "audio-3.0.wav:3.0:0.1|0.2|0.3" "audio-quad.wav:quad:0.1|0.2|0.3|0.4" \
  "audio-lcr.mov:FC+FL+FR:0.3|0.1|0.2" "audio-quad.mov:quad:0.1|0.2|0.3|0.4" \
  "audio-5.1.4.wav:5.1.4:0.05|0.1|0.15|0.2|0.25|0.3|0.35|0.4|0.45|0.5"; do
  IFS=: read -r name layout levels <<<"$entry"
  "${FFMPEG[@]}" -f lavfi -i "aevalsrc=${levels}:c=${layout}:s=48000:d=0.1" \
    -c:a pcm_s16le -flags:a +bitexact "$name"
done
# Speakers a codec's own header declares: AAC 3.0 and 5.1, whose bitstreams order their channels
# C L R (Ls Rs LFE) and whose decoders deliver FFmpeg's order. Each speaker is a tone of its own
# (FL 300 Hz, FR 500, FC 700, LFE 60, BL 900, BR 1100), so a decoded plane says which speaker it is.
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=0.3*sin(2*PI*300*t)|0.3*sin(2*PI*500*t)|0.3*sin(2*PI*700*t):c=3.0:s=48000:d=0.5" \
  -c:a aac -b:a 192k -flags:a +bitexact audio-aac-3.0.mp4
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=0.3*sin(2*PI*300*t)|0.3*sin(2*PI*500*t)|0.3*sin(2*PI*700*t)|0.3*sin(2*PI*60*t)|0.3*sin(2*PI*900*t)|0.3*sin(2*PI*1100*t):c=5.1:s=48000:d=0.5" \
  -c:a aac -b:a 320k -flags:a +bitexact audio-aac-5.1.mp4
# Speakers declared outside an AAC channel configuration, each a tone of its own and laid out unlike
# the count's default: AAC quad (PCE), AAC 7.1, ADTS 3.0, AC-3 5.1, E-AC-3 5.0, DTS 5.1, FLAC 4.0.
tones() {
  local speaker expression=""
  for speaker in "$@"; do
    case "$speaker" in
      FL) frequency=300 ;; FR) frequency=500 ;; FC) frequency=700 ;; LFE) frequency=60 ;;
      BL) frequency=900 ;; BR) frequency=1100 ;; SL) frequency=1300 ;; SR) frequency=1500 ;;
      BC) frequency=1700 ;;
    esac
    expression+="${expression:+|}0.3*sin(2*PI*${frequency}*t)"
  done
  echo "$expression"
}
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=$(tones FL FR BL BR):c=quad:s=48000:d=0.5" \
  -c:a aac -b:a 256k -flags:a +bitexact audio-aac-quad.mp4
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=$(tones FL FR FC LFE BL BR SL SR):c=7.1:s=48000:d=0.5" \
  -c:a aac -b:a 448k -flags:a +bitexact audio-aac-7.1.mp4
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=$(tones FL FR FC):c=3.0:s=48000:d=0.5" \
  -c:a aac -b:a 192k -flags:a +bitexact -f adts audio-adts-3.0.aac
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=$(tones FL FR FC LFE SL SR):c=5.1(side):s=48000:d=0.5" \
  -c:a ac3 -b:a 384k -flags:a +bitexact audio-ac3-5.1.mp4
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=$(tones FL FR FC SL SR):c=5.0(side):s=48000:d=0.5" \
  -c:a eac3 -b:a 256k -flags:a +bitexact audio-eac3-5.0.mp4
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=$(tones FL FR FC LFE SL SR):c=5.1(side):s=48000:d=0.3" \
  -c:a dca -strict -2 -flags:a +bitexact -fflags +bitexact audio-dts-5.1.mkv
# FFmpeg warns that FLAC names no 4.0; the muxer declares it in the comment.
for name in audio-flac-4.0.flac audio-flac-4.0.mkv; do
  "${FFMPEG[@]}" -f lavfi -i "aevalsrc=$(tones FL FR FC BC):c=4.0:s=48000:d=0.3" \
    -loglevel fatal -c:a flac -flags:a +bitexact -fflags +bitexact "$name"
done
# Speakers an ISO/IEC 23001-8 `chnl` box declares for PCM in MP4 (`ipcm`), each speaker at its own
# level: C L R as the box's channel configuration 3, and quad as explicit speaker positions.
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=0.3|0.1|0.2:c=FC+FL+FR:s=48000:d=0.1" \
  -c:a pcm_s16le -flags:a +bitexact -f mp4 audio-ipcm-lcr.mp4
"${FFMPEG[@]}" -f lavfi -i "aevalsrc=0.1|0.2|0.3|0.4:c=quad:s=48000:d=0.1" \
  -c:a pcm_s16le -flags:a +bitexact -f mp4 audio-ipcm-quad.mp4

"${FFMPEG[@]}" -f lavfi -i testsrc=size=65x49:rate=12:duration=0.4 \
  -an -c:v ffv1 -pix_fmt yuv444p odd-dimensions.avi

"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=25:duration=0.4 \
  -an -c:v mpeg4 -pix_fmt yuv420p -movflags +write_colr \
  -color_primaries smpte170m -color_trc smpte170m -colorspace smpte170m -color_range pc bt601-full.mp4

"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=25:duration=0.4 \
  -an -c:v mpeg4 -pix_fmt yuv420p missing-color-metadata.mp4

# A layout the renderer does not sample: ProRes 4444 tagged PQ BT.2020, the picture of `hdr-pq.mp4`
# at 4:4:4, with an alpha plane (opaque but for half alpha over the first two colour blocks) and
# without one, which FFmpeg decodes as yuva444p12le and yuv444p12le.
for alpha in alpha: opaque:; do
  name="${alpha%%:*}"
  "${FFMPEG[@]}" -f lavfi -i "color=black:size=64x48:rate=30:duration=0.1,format=yuva444p10le,geq=\
lum='if(lt(Y,32),64+876*X/63,if(lt(X,16),300,if(lt(X,32),500,if(lt(X,48),650,800))))':\
cb='if(lt(Y,32),512,if(lt(X,16),420,if(lt(X,32),600,if(lt(X,48),460,560))))':\
cr='if(lt(Y,32),512,if(lt(X,16),620,if(lt(X,32),430,if(lt(X,48),560,480))))':\
a='if(lt(Y,32),1023,if(lt(X,32),512,1023))',\
setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=tv" \
    -an -c:v prores_ks -profile:v 4444 \
    $([[ "$name" == alpha ]] && echo "-pix_fmt yuva444p10le -alpha_bits 16" || echo "-pix_fmt yuv444p10le") \
    -flags:v +bitexact "prores-pq-${name}.mov"
done

# Host parity uses identical pixels: PNG frames for the browser and QuickTime containing those
# frames for native decoding.
mkdir -p parity-frames
"${FFMPEG[@]}" -f lavfi -i testsrc2=size=96x64:rate=12:duration=1 \
  -pix_fmt rgb24 -flags:v +bitexact -start_number 0 parity-frames/%02d.png
"${FFMPEG[@]}" -framerate 12 -start_number 0 -i parity-frames/%02d.png \
  -c:v png -pix_fmt rgb24 -fflags +bitexact -flags:v +bitexact parity-rgb.mov
"${FFMPEG[@]}" -f lavfi -i rgbtestsrc=size=96x64 -frames:v 1 \
  -pix_fmt rgb24 -flags:v +bitexact parity-image.png
# The effects parity project's 3D LUT: a smooth, non-linear grade over 5^3 entries, red fastest.
python3 - <<'LUT'
lines = ['TITLE "alpha2 parity grade"', "LUT_3D_SIZE 5"]
for b in range(5):
    for g in range(5):
        for r in range(5):
            red, green, blue = r / 4, g / 4, b / 4
            grade = (
                min(1, red * 0.9 + 0.08 + 0.1 * green * blue),
                green**1.3 * 0.95 + 0.03,
                blue * 0.8 + 0.12 * red,
            )
            lines.append(" ".join(f"{value:.6f}" for value in grade))
open("parity-grade.cube", "w").write("\n".join(lines) + "\n")
LUT

# RGB outside BT.709's gamut: solid red in planar RGB (FFV1), tagged by `setparams` (output options
# alone leave the primaries unwritten in some versions); `transcode` must not encode it as YUV.
for entry in rgb-bt2020:gbrp:bt2020 rgb-smpte170m:gbrp:smpte170m rgb10-bt2020:gbrp10le:bt2020; do
  IFS=: read -r name format primaries <<<"$entry"
  "${FFMPEG[@]}" -f lavfi -i color=c=red:size=64x48:rate=25:duration=0.2 \
    -vf "format=$format,setparams=color_primaries=$primaries:color_trc=bt709:colorspace=gbr:range=pc" \
    -an -c:v ffv1 -fflags +bitexact -flags:v +bitexact "$name.mkv"
done

# Wide-gamut SDR: BT.2020 with BT.709's transfer, lossless 10-bit limited, four flat blocks (two
# inside BT.709's gamut, one outside, one skin tone) so the conversion is checked from exact codes.
"${FFMPEG[@]}" -f lavfi -i "color=black:size=64x48:rate=25:duration=0.2,format=yuv420p10le,geq=\
lum='if(lt(Y,24),if(lt(X,32),470,520),if(lt(X,32),658,600))':\
cb='if(lt(Y,12),if(lt(X,16),512,470),if(lt(X,16),189,430))':\
cr='if(lt(Y,12),if(lt(X,16),512,440),if(lt(X,16),100,600))',\
setparams=color_primaries=bt2020:color_trc=bt2020-10:colorspace=bt2020nc:range=tv" \
  -an -c:v ffv1 -fflags +bitexact -flags:v +bitexact bt2020-sdr.mkv

# Rates whose exact frame length an MPEG-4 Part 2 time base cannot hold (its denominator is 16
# bits): 119.88 and 239.76 fps, and a variable-rate recording in 1/60000 whose 12 frames FFmpeg
# averages to 360000/34913 fps.
for rate in 120000 240000; do
  "${FFMPEG[@]}" -f lavfi -i "testsrc2=size=64x48:rate=$rate/1001" -frames:v 30 \
    -an -c:v mpeg4 -video_track_timescale "$rate" -fflags +bitexact -flags:v +bitexact \
    "rate-$((rate / 1000))-1001.mp4"
done
"${FFMPEG[@]}" -f lavfi -i testsrc2=size=64x48:rate=30 -frames:v 12 \
  -vf "settb=1/60000,setpts='N*N*439+N*1337'" -fps_mode passthrough -enc_time_base 1/60000 \
  -an -c:v mpeg4 -video_track_timescale 60000 -fflags +bitexact -flags:v +bitexact vfr-average.mp4

# Non-square pixels: 60x48 stored at 16:15 (PAL's 4:3), shown 64x48, and the same packets with a
# display matrix (`-display_rotation 90`), shown 48x64.
"${FFMPEG[@]}" -f lavfi -i testsrc2=size=60x48:rate=25:duration=0.2 -vf setsar=16/15 \
  -an -c:v mpeg4 -pix_fmt yuv420p -fflags +bitexact -flags:v +bitexact sar-16-15.mp4
"${FFMPEG[@]}" -display_rotation:v:0 90 -i sar-16-15.mp4 -c copy -fflags +bitexact sar-16-15-rotation.mp4

# A clock that does not start at zero: 150 intra frames whose luma is 16 + their index, 25 fps, in
# MPEG-TS with `-output_ts_offset 100`, so the first frame is at 101.4 s (the muxer's 1.4 s delay on
# top) and frame 125 at 106.4 s.
"${FFMPEG[@]}" -f lavfi -i "nullsrc=s=64x48:r=25,format=yuv420p,geq=lum='16+N':cb=128:cr=128" \
  -frames:v 150 -an -c:v mpeg2video -q:v 1 -g 1 -output_ts_offset 100 -fflags +bitexact \
  -flags:v +bitexact ts-offset-100.ts

# Streams a GPU decoder takes, which need encoders the LGPL SDK does not ship: point
# FFMPEG_ENCODERS at any FFmpeg with libx264 and libvpx.
if [[ -n "${FFMPEG_ENCODERS:-}" ]]; then
  ENCODE=("$FFMPEG_ENCODERS" -hide_banner -loglevel error -y -fflags +bitexact)
  # 8-bit 4:2:0 High profile with B-frames and four references: every desktop hardware decoder takes
  # it.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=128x96:rate=30:duration=1 \
    -an -c:v libx264 -profile:v high -pix_fmt yuv420p -g 30 -bf 2 -refs 4 \
    -x264-params threads=1:sliced-threads=0 h264-long-gop.mp4
  # 1920x1080: the size at which a browser's hardware decoder is the one it picks, so the editor
  # package check reports whether that decoder fell back to software.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=1920x1080:rate=30:duration=0.5 \
    -an -c:v libx264 -profile:v high -pix_fmt yuv420p -crf 30 \
    -x264-params threads=1:sliced-threads=0 -flags:v +bitexact h264-1080p.mp4
  # High 10: FFmpeg's H.264 decoder offers a hardware format for it, and NVDEC then fails the
  # session on the first packet, not at open.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=128x96:rate=30:duration=0.5 \
    -an -c:v libx264 -profile:v high10 -pix_fmt yuv420p10le \
    -x264-params threads=1:sliced-threads=0 h264-high10.mp4
  # FFmpeg registers a Vulkan VP9 decoder; a device without VP9 decode fails the session on the
  # first packet, not at open.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=128x96:rate=30:duration=0.5 \
    -an -c:v libvpx-vp9 -b:v 200k -pix_fmt yuv420p -threads 1 \
    -fflags +bitexact -flags:v +bitexact vp9.webm
  # Browser colour contract: a gray ramp over four flat, in-gamut colour blocks, losslessly coded
  # and tagged BT.709 limited, so the browser's conversion of a decoded frame can be compared with
  # the frame's own planes.
  "${ENCODE[@]}" -f lavfi -i "color=gray:size=256x64:rate=30:duration=0.5,format=yuv420p,geq=\
lum='if(lt(Y,32),16+219*X/255,if(lt(X,64),110,if(lt(X,128),145,if(lt(X,192),80,170))))':\
cb='if(lt(Y,16),128,if(lt(X,32),100,if(lt(X,64),160,if(lt(X,96),150,120))))':\
cr='if(lt(Y,16),128,if(lt(X,32),170,if(lt(X,64),100,if(lt(X,96),120,140))))'" \
    -an -c:v libvpx-vp9 -lossless 1 -pix_fmt yuv420p -threads 1 \
    -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv \
    -fflags +bitexact -flags:v +bitexact vp9-color.webm
  # Packet copy: tagged BT.709 limited, closed 30-frame GOPs of IDR pictures with two B-frames each,
  # and AAC stereo at 48 kHz, so a whole-file copy carries audio and a GOP-aligned trim copies video
  # alone.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=128x96:rate=30:duration=2 \
    -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
    -c:v libx264 -profile:v high -pix_fmt yuv420p -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 \
    -x264-params threads=1:sliced-threads=0 \
    -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv \
    -c:a aac -ac 2 -b:a 96k -shortest h264-bt709-copy.mp4
  # Packet copy refusals: what a copy would carry and a render does not apply. The same stream
  # mirrored by its display matrix; and declaring 4:3 pixels only in its SPS VUI, with the
  # container's `pasp` box renamed `free` so nothing else declares them.
  "${FFMPEG[@]}" -display_hflip -i h264-bt709-copy.mp4 -c copy h264-bt709-hflip.mp4
  # Display matrices that mirror: the same stream flipped top to bottom, and turned a quarter
  # counter-clockwise then flipped left to right (a transverse), with the pictures FFmpeg's
  # autorotate shows for them and the left-to-right flip.
  "${FFMPEG[@]}" -display_vflip -i h264-bt709-copy.mp4 -c copy h264-bt709-vflip.mp4
  "${FFMPEG[@]}" -display_rotation 90 -display_hflip -i h264-bt709-copy.mp4 -c copy h264-bt709-transverse.mp4
  # Matroska states the same as a projection pose: a roll, and a yaw of 180 degrees for the mirror.
  "${FFMPEG[@]}" -i h264-bt709-transverse.mp4 -map 0:v -c copy -fflags +bitexact h264-bt709-transverse.mkv
  for mirrored in hflip vflip transverse; do
    "${FFMPEG[@]}" -i "h264-bt709-${mirrored}.mp4" -frames:v 1 -pix_fmt rgb24 -bitexact "h264-bt709-${mirrored}-autorotate.png"
  done
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -c copy -bsf:v h264_metadata=sample_aspect_ratio=4/3 .vui-sar.mp4
  python3 - <<'PATCH'
data = bytearray(open(".vui-sar.mp4", "rb").read())
box = data.find(b"pasp")
assert box > 0 and data.find(b"pasp", box + 1) == -1
data[box : box + 4] = b"free"
open("h264-bt709-vui-sar.mp4", "wb").write(data)
PATCH
  rm -f .vui-sar.mp4
  # Pixel aspect ratios as FFmpeg reads them: the same VUI-only stream in MPEG-TS, which declares no
  # ratio; a `pasp` box declaring 4:3 over the SPS's 1:1, which wins; and a broadcast-style stream
  # whose SPS says 4:3 for the first GOP and 1:1 from the second.
  "${FFMPEG[@]}" -i h264-bt709-vui-sar.mp4 -map 0:v -c copy -f mpegts h264-bt709-vui-sar.ts
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy -aspect 16:9 h264-bt709-pasp.mp4
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy -frames:v 30 -bsf:v h264_metadata=sample_aspect_ratio=4/3 .sar-first.mp4
  "${FFMPEG[@]}" -ss 1 -i h264-bt709-copy.mp4 -map 0:v -c copy .sar-second.mp4
  printf "file .sar-first.mp4\nfile .sar-second.mp4\n" > .sar-change.txt
  "${FFMPEG[@]}" -f concat -safe 0 -i .sar-change.txt -c copy -f mpegts h264-bt709-sar-change.ts
  rm -f .sar-first.mp4 .sar-second.mp4 .sar-change.txt
  # Seeks the demuxer cannot land exactly: the same video in MPEG-TS, which indexes every packet as
  # a keyframe and starts at the 1.4 s mux delay, and in Matroska, whose millisecond time base
  # rounds a time just before a keyframe onto it.
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy -f mpegts h264-bt709-copy.ts
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy -fflags +bitexact h264-bt709-copy.mkv
  # Decoded geometry and siting the SPS declares: the same stream cropping 8 columns on the left
  # (FFmpeg rounds a left crop below its alignment away unless told not to), and with no VUI at all,
  # so its chroma sits left by inference while FFmpeg reports it unspecified.
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy -bsf:v h264_metadata=crop_left=8 h264-bt709-crop-left.mp4
  # A bottom crop: 128x90 shown of a 128x96 coded picture, so a hardware surface is taller than the
  # picture, as a 1080p stream's 1088 rows are.
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy -bsf:v h264_metadata=crop_bottom=6 h264-bt709-crop-bottom.mp4
  # Colour a copy must not take for BT.709: the VUI keeps its video signal type (limited range) but
  # leaves primaries, transfer and matrix unspecified, and the `colr` box is renamed `free`.
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy \
    -bsf:v h264_metadata=colour_primaries=2:transfer_characteristics=2:matrix_coefficients=2 .range-only.mp4
  python3 - <<'PATCH'
data = bytearray(open(".range-only.mp4", "rb").read())
box = data.find(b"colr")
assert box > 0 and data.find(b"colr", box + 1) == -1
data[box : box + 4] = b"free"
open("h264-bt709-range-only.mp4", "wb").write(data)
PATCH
  rm -f .range-only.mp4
  # Audio that outlasts the video: the 2 s video beside 2.5 s of AAC, muxed without -shortest.
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2.5 \
    -map 0:v -map 1:a -c:v copy -c:a aac -ac 2 -b:a 96k -flags:a +bitexact h264-bt709-audio-longer.mp4
  # The same pictures under other parameter sets: level 4.1 in the SPS. Then two sample
  # descriptions: the video's second second described by that `avc1` entry (`two-stsd.py`).
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy -bsf:v h264_metadata=level=4.1 h264-bt709-level.mp4
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy .first.mp4
  python3 "$here/two-stsd.py" .first.mp4 h264-bt709-level.mp4 h264-bt709-two-stsd.mp4
  rm -f .first.mp4
  "${FFMPEG[@]}" -i h264-bt709-copy.mp4 -map 0:v -c copy .no-vui.mp4
  python3 "$here/no-vui.py" .no-vui.mp4 h264-bt709-no-vui.mp4
  rm -f .no-vui.mp4
  # AV1, which the LGPL SDK decodes only through a hardware session: its one AV1 decoder is FFmpeg's
  # own.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=64x48:rate=30:duration=0.2 \
    -an -c:v libaom-av1 -cpu-used 8 -row-mt 0 -threads 1 -pix_fmt yuv420p \
    -flags:v +bitexact av1.mp4
  # YUV transcode: tagged BT.709 limited and large enough for a hardware encoder, so a clip shown as
  # it is reaches one as its decoded planes.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=256x144:rate=30:duration=0.5 \
    -an -c:v libx264 -profile:v high -pix_fmt yuv420p \
    -x264-params threads=1:sliced-threads=0 \
    -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv \
    h264-bt709-transcode.mp4
  # A picture size and left crop that change mid-stream, as a broadcast splice brings them: the
  # 256x144 transcode, then the 128x96 crop-left stream, joined in MPEG-TS so the second SPS
  # arrives in band. A decoder whose surface is the visible size (D3D12VA) cannot hold the second.
  printf "file h264-bt709-transcode.mp4\nfile h264-bt709-crop-left.mp4\n" > .res-change.txt
  "${FFMPEG[@]}" -f concat -safe 0 -i .res-change.txt -map 0:v -c copy -f mpegts h264-bt709-res-change.ts
  rm -f .res-change.txt
  # Hardware import: 320x180 in each codec a Vulkan decoder takes, 8-bit and 10-bit (P010), a height
  # no codec's block size divides, so every decoder surface is padded below the picture.
  for spec in h264-padded:libx264:yuv420p hevc-padded:libx265:yuv420p hevc-main10-padded:libx265:yuv420p10le \
    av1-padded:libaom-av1:yuv420p av1-10bit-padded:libaom-av1:yuv420p10le; do
    IFS=: read -r name codec format <<<"$spec"
    case "$codec" in
      libx264) options=(-profile:v high -x264-params threads=1:sliced-threads=0) ;;
      libx265) options=(-x265-params log-level=0:pools=1:frame-threads=1) ;;
      libaom-av1) options=(-cpu-used 8 -row-mt 0 -threads 1) ;;
    esac
    "${ENCODE[@]}" -f lavfi -i testsrc2=size=320x180:rate=30:duration=0.5 \
      -an -c:v "$codec" "${options[@]}" -pix_fmt "$format" \
      -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv \
      -flags:v +bitexact "$name.mp4"
  done
  # A phone's HDR video in miniature: HEVC Main 10, BT.2020, HLG, the colour in both the VUI and
  # `colr`. Browsers that decode HEVC only to opaque hardware frames cannot hand it to the engine.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 12 \
    -an -c:v libx265 -x265-params log-level=0:pools=1:frame-threads=1 -pix_fmt yuv420p10le \
    -color_primaries bt2020 -color_trc arib-std-b67 -colorspace bt2020nc -color_range tv \
    -movflags +write_colr -flags:v +bitexact hevc-main10-hlg.mp4
  # Row padding on every architecture: 336 is a width under each decoder's stride alignment, so a
  # decoded row is always followed by bytes the decoder never wrote.
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=336x180:rate=30:duration=0.5 \
    -an -c:v libx264 -profile:v high -x264-params threads=1:sliced-threads=0 -pix_fmt yuv420p \
    -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv \
    -flags:v +bitexact h264-row-padded.mp4
  # HDR: losslessly coded 10-bit BT.2020 limited, a luma ramp (Y 64 to 940) over a band of four
  # colours, tagged PQ and HLG, so decode and tone map can be checked against the reference from the
  # codes alone.
  for transfer in smpte2084:pq arib-std-b67:hlg; do
    "${ENCODE[@]}" -f lavfi -i "color=black:size=64x48:rate=30:duration=0.2,format=yuv420p10le,geq=\
lum='if(lt(Y,32),64+876*X/63,if(lt(X,16),300,if(lt(X,32),500,if(lt(X,48),650,800))))':\
cb='if(lt(Y,16),512,if(lt(X,8),420,if(lt(X,16),600,if(lt(X,24),460,560))))':\
cr='if(lt(Y,16),512,if(lt(X,8),620,if(lt(X,16),430,if(lt(X,24),560,480))))'" \
      -an -c:v libx264 -qp 0 -pix_fmt yuv420p10le -x264-params threads=1:sliced-threads=0 \
      -color_primaries bt2020 -color_trc "${transfer%%:*}" -colorspace bt2020nc -color_range tv \
      -movflags +write_colr "hdr-${transfer##*:}.mp4"
  done
  # Colour a project declares: the PQ stream with its VUI colour description unspecified and its
  # `colr` box renamed `free`, so only a declaration says it is PQ.
  "${FFMPEG[@]}" -i hdr-pq.mp4 -map 0:v -c copy \
    -bsf:v h264_metadata=colour_primaries=2:transfer_characteristics=2:matrix_coefficients=2 \
    .untagged.mp4
  python3 - <<'PATCH'
data = bytearray(open(".untagged.mp4", "rb").read())
box = data.find(b"colr")
assert box > 0 and data.find(b"colr", box + 1) == -1
data[box : box + 4] = b"free"
open("hdr-pq-untagged.mp4", "wb").write(data)
PATCH
  rm -f .untagged.mp4
  # Chroma siting a stream signals: the PQ stream with chroma_sample_loc_type 2 (top-left, as UHD
  # HDR masters use) in its VUI, an HEVC stream that says the same, and the AV1 stream with
  # chroma_sample_position colocated.
  "${FFMPEG[@]}" -i hdr-pq.mp4 -map 0:v -c copy -bsf:v h264_metadata=chroma_sample_loc_type=2 hdr-pq-topleft.mp4
  "${ENCODE[@]}" -f lavfi -i testsrc2=size=64x48:rate=30:duration=0.2 \
    -an -c:v libx265 -pix_fmt yuv420p -x265-params log-level=0:pools=1:frame-threads=1:chromaloc=2 \
    -flags:v +bitexact hevc-topleft.mp4
  "${FFMPEG[@]}" -i av1.mp4 -map 0:v -c copy -bsf:v av1_metadata=chroma_sample_position=colocated av1-colocated.mp4
  # The HEVC stream again, its sample entry `hvc1` (parameter sets in `hvcC` only), as Apple devices
  # write it; `hevc-topleft.mp4` is `hev1`, FFmpeg's default.
  "${FFMPEG[@]}" -i hevc-topleft.mp4 -map 0:v -c copy -tag:v hvc1 hevc-hvc1.mp4
  # Frame times a time base rounds or holds exactly: H.264 frames whose luma is 16 + 3 * index (16 +
  # 2 * index when joined), at 29.97, 30 and 60 fps in Matroska, MP4, 1/600, 1/100 and 1/90000.
  ROUND=(-an -c:v libx264 -profile:v high -pix_fmt yuv420p -qp 4 -bf 0 -g 30
    -x264-params threads=1:sliced-threads=0 -fflags +bitexact -flags:v +bitexact)
  index() { echo "nullsrc=s=64x48:r=$1,format=yuv420p,geq=lum='16+$2*N':cb=128:cr=128"; }
  "${ENCODE[@]}" -f lavfi -i "$(index 30000/1001 3)" -frames:v 60 "${ROUND[@]}" frame-rounding-2997.mkv
  "${FFMPEG[@]}" -i frame-rounding-2997.mkv -c copy frame-rounding-2997-remux.mp4
  for scale in 600 100; do
    "${ENCODE[@]}" -f lavfi -i "$(index 30000/1001 3)" -frames:v 60 "${ROUND[@]}" \
      -video_track_timescale "$scale" "frame-rounding-2997-$scale.mp4"
  done
  "${ENCODE[@]}" -f lavfi -i "$(index 30 3)" -frames:v 60 "${ROUND[@]}" -video_track_timescale 600 \
    frame-rounding-30-600.mp4
  mixed="settb=1/90000,setpts='if(lt(N,20),N*1500,if(lt(N,40),30000+(N-20)*3000,90000+(N-40)*1500))'"
  "${ENCODE[@]}" -f lavfi -i "$(index 60 3)" -frames:v 60 -vf "$mixed" -vsync passthrough \
    -enc_time_base 1/90000 "${ROUND[@]}" -video_track_timescale 90000 frame-rounding-vfr-90000.mp4
  "${ENCODE[@]}" -f lavfi -i "$(index 60 3)" -frames:v 60 -vf "$mixed" -vsync passthrough \
    -enc_time_base 1/90000 "${ROUND[@]}" frame-rounding-vfr-ms.mkv
  "${ENCODE[@]}" -f lavfi -i "$(index 30000/1001 2)" -frames:v 96 \
    -vf "settb=1/90000,setpts='if(lt(N,64),N*3003,90*round(N*1001/30))'" -vsync passthrough \
    -enc_time_base 1/90000 "${ROUND[@]}" -video_track_timescale 90000 frame-rounding-joined.mp4
  "${ENCODE[@]}" -f lavfi -i "$(index 30 3)" -frames:v 64 "${ROUND[@]}" -g 32 -keyint_min 32 \
    -sc_threshold 0 frame-rounding-30-gop32.mkv
  # 23.976 fps in Matroska's milliseconds (41 and 42 ms), the same packets copied into an MP4, the
  # same frames in MPEG-TS, whose 90 kHz rounds them too (3753 and 3754), and 60 fps VP9 in WebM's
  # milliseconds (16 and 17 ms): with the files above, every container at 23.976 to 60 fps.
  "${ENCODE[@]}" -f lavfi -i "$(index 24000/1001 3)" -frames:v 60 "${ROUND[@]}" \
    frame-rounding-23976.mkv
  "${FFMPEG[@]}" -i frame-rounding-23976.mkv -c copy frame-rounding-23976-remux.mp4
  "${ENCODE[@]}" -f lavfi -i "$(index 24000/1001 3)" -frames:v 60 "${ROUND[@]}" -muxdelay 0 \
    -muxpreload 0 -f mpegts frame-rounding-23976.ts
  "${ENCODE[@]}" -f lavfi -i "$(index 60 3)" -frames:v 60 -an -c:v libvpx-vp9 -lossless 1 \
    -pix_fmt yuv420p -g 30 -auto-alt-ref 0 -lag-in-frames 0 -threads 1 -fflags +bitexact \
    -flags:v +bitexact frame-rounding-60.webm
  # The audio seek tones (above) in WebM through libopus, whose pre-skip is 312 samples.
  "${ENCODE[@]}" -f lavfi -i "$SEEK_TONES:s=48000" -c:a libopus -b:a 96k -fflags +bitexact \
    -flags:a +bitexact audio-seek-libopus.webm
  # Speakers Opus mapping family 1 and Vorbis declare by their channel count: 5.1, which their
  # bitstreams order L C R Ls Rs LFE (Vorbis's order) and FFmpeg's decoders deliver in FFmpeg's,
  # each speaker the tone of its own above, through libopus in WebM and libvorbis in Ogg.
  "${ENCODE[@]}" -f lavfi -i "aevalsrc=$(tones FL FR FC LFE BL BR):c=5.1:s=48000:d=0.3" \
    -c:a libopus -mapping_family 1 -b:a 192k -fflags +bitexact -flags:a +bitexact audio-opus-5.1.webm
  "${ENCODE[@]}" -f lavfi -i "aevalsrc=$(tones FL FR FC LFE BL BR):c=5.1:s=48000:d=0.3" \
    -c:a libvorbis -q:a 3 -fflags +bitexact -flags:a +bitexact audio-vorbis-5.1.ogg
fi

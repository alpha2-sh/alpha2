#!/usr/bin/env python3
# Encodes raw PCM with libfdk-aac (AAC-LC) in an AAC channel configuration FFmpeg's own encoder
# cannot write (11, 12, 14): what `generate-decoder-order-fixtures.mjs` measures those with.
#
#   fdk-aac-encode.py <configuration> <channels> <bitrate> < s16le-interleaved > json
#
# Reads interleaved s16 at 48 kHz in libfdk-aac's MPEG channel order and writes JSON: the
# AudioSpecificConfig and every access unit, hex. Needs libfdk-aac.so.2 (fdk-aac 2.0+); it has no
# headers, so the API is declared here.
import ctypes
import json
import sys

AACENC_AOT = 0x0100
AACENC_BITRATE = 0x0101
AACENC_SAMPLERATE = 0x0103
AACENC_CHANNELMODE = 0x0106
AACENC_CHANNELORDER = 0x0107
AACENC_TRANSMUX = 0x0300
IN_AUDIO_DATA = 0
OUT_BITSTREAM_DATA = 3
AACENC_OK = 0
AACENC_ENCODE_EOF = 0x0080


class BufDesc(ctypes.Structure):
    _fields_ = [
        ("numBufs", ctypes.c_int),
        ("bufs", ctypes.POINTER(ctypes.c_void_p)),
        ("bufferIdentifiers", ctypes.POINTER(ctypes.c_int)),
        ("bufSizes", ctypes.POINTER(ctypes.c_int)),
        ("bufElSizes", ctypes.POINTER(ctypes.c_int)),
    ]


class InArgs(ctypes.Structure):
    _fields_ = [("numInSamples", ctypes.c_int), ("numAncBytes", ctypes.c_int)]


class OutArgs(ctypes.Structure):
    _fields_ = [
        ("numOutBytes", ctypes.c_int),
        ("numInSamples", ctypes.c_int),
        ("numAncBytes", ctypes.c_int),
        ("bitResState", ctypes.c_int),
    ]


class InfoStruct(ctypes.Structure):
    _fields_ = [
        ("maxOutBufBytes", ctypes.c_uint),
        ("maxAncBytes", ctypes.c_uint),
        ("inBufFillLevel", ctypes.c_uint),
        ("inputChannels", ctypes.c_uint),
        ("frameLength", ctypes.c_uint),
        ("nDelay", ctypes.c_uint),
        ("nDelayCore", ctypes.c_uint),
        ("confBuf", ctypes.c_ubyte * 64),
        ("confSize", ctypes.c_uint),
    ]


def describe(buffer, identifier, element_size):
    pointer = ctypes.c_void_p(ctypes.addressof(buffer))
    return BufDesc(
        1,
        ctypes.pointer(pointer),
        ctypes.pointer(ctypes.c_int(identifier)),
        ctypes.pointer(ctypes.c_int(ctypes.sizeof(buffer))),
        ctypes.pointer(ctypes.c_int(element_size)),
    )


def main():
    configuration, channels, bitrate = (int(value) for value in sys.argv[1:4])
    fdk = ctypes.CDLL("libfdk-aac.so.2")
    handle = ctypes.c_void_p()
    check = lambda error, what: error == AACENC_OK or sys.exit(f"{what}: 0x{error:x}")
    check(fdk.aacEncOpen(ctypes.byref(handle), 0, channels), "aacEncOpen")
    for parameter, value in [
        (AACENC_AOT, 2),
        (AACENC_SAMPLERATE, 48_000),
        (AACENC_CHANNELMODE, configuration),
        (AACENC_CHANNELORDER, 0),
        (AACENC_BITRATE, bitrate),
        (AACENC_TRANSMUX, 0),
    ]:
        check(fdk.aacEncoder_SetParam(handle, parameter, value), f"parameter 0x{parameter:x}")
    check(fdk.aacEncEncode(handle, None, None, None, None), "initialization")
    info = InfoStruct()
    check(fdk.aacEncInfo(handle, ctypes.byref(info)), "aacEncInfo")
    pcm = sys.stdin.buffer.read()
    samples = (ctypes.c_int16 * (len(pcm) // 2)).from_buffer_copy(pcm)
    output = (ctypes.c_ubyte * info.maxOutBufBytes)()
    units = []
    offset = 0
    while True:
        remaining = len(samples) - offset
        chunk = (
            (ctypes.c_int16 * remaining).from_buffer(samples, offset * 2)
            if remaining > 0
            else (ctypes.c_int16 * 1)()
        )
        in_args = InArgs(remaining if remaining > 0 else -1, 0)
        out_args = OutArgs()
        error = fdk.aacEncEncode(
            handle,
            ctypes.byref(describe(chunk, IN_AUDIO_DATA, 2)),
            ctypes.byref(describe(output, OUT_BITSTREAM_DATA, 1)),
            ctypes.byref(in_args),
            ctypes.byref(out_args),
        )
        if error == AACENC_ENCODE_EOF:
            break
        check(error, "aacEncEncode")
        offset += out_args.numInSamples
        if out_args.numOutBytes > 0:
            units.append(bytes(output[: out_args.numOutBytes]).hex())
    fdk.aacEncClose(ctypes.byref(handle))
    config = bytes(info.confBuf[: info.confSize]).hex()
    json.dump({"config": config, "delay": info.nDelay, "units": units}, sys.stdout)


main()

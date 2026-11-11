#!/usr/bin/env python3
"""Encodes a mono 16-bit WAV file to AMR (RFC 4867 section 5 storage format).

Usage: amr_encode.py IN.wav OUT.amr nb|wb

ffmpeg decodes AMR natively but encodes it only when built with
libopencore-amrnb and libvo-amrwbenc, which many builds are not. This
script calls those libraries directly (Debian/Ubuntu: libopencore-amrnb0
and libvo-amrwbenc0), and ffmpeg then remuxes the result (-c copy).

AMR-NB wants 8 kHz input and is encoded at 12.2 kbit/s (mode MR122),
AMR-WB 16 kHz at 23.85 kbit/s (mode 8); both without DTX, 20 ms frames.
"""
import ctypes
import ctypes.util
import sys
import wave

src, dst, kind = sys.argv[1], sys.argv[2], sys.argv[3]
with wave.open(src, "rb") as w:
    assert w.getnchannels() == 1 and w.getsampwidth() == 2, "mono 16-bit only"
    rate = w.getframerate()
    pcm = w.readframes(w.getnframes())

if kind == "nb":
    assert rate == 8000, "AMR-NB needs 8 kHz"
    lib = ctypes.CDLL(ctypes.util.find_library("opencore-amrnb") or "libopencore-amrnb.so.0")
    lib.Encoder_Interface_init.restype = ctypes.c_void_p
    lib.Encoder_Interface_init.argtypes = [ctypes.c_int]
    lib.Encoder_Interface_Encode.argtypes = [
        ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_int]
    state = lib.Encoder_Interface_init(0)
    encode = lambda frame, out: lib.Encoder_Interface_Encode(state, 7, frame, out, 0)
    magic, n = b"#!AMR\n", 160
else:
    assert rate == 16000, "AMR-WB needs 16 kHz"
    lib = ctypes.CDLL(ctypes.util.find_library("vo-amrwbenc") or "libvo-amrwbenc.so.0")
    lib.E_IF_init.restype = ctypes.c_void_p
    lib.E_IF_encode.argtypes = [
        ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_int]
    state = lib.E_IF_init()
    encode = lambda frame, out: lib.E_IF_encode(state, 8, frame, out, 0)
    magic, n = b"#!AMR-WB\n", 320

out = bytearray(magic)
buf = ctypes.create_string_buffer(64)
# The last frame is padded with silence.
pcm += bytes(-len(pcm) % (2 * n))
for i in range(0, len(pcm), 2 * n):
    frame = ctypes.create_string_buffer(pcm[i : i + 2 * n], 2 * n)
    size = encode(frame, buf)
    assert size > 0, "encoder failed"
    out += buf.raw[:size]
with open(dst, "wb") as f:
    f.write(out)

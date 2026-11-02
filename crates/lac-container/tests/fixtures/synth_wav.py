#!/usr/bin/env python3
"""Writes a deterministic test WAV: per-channel sine plus low-level noise.

Usage: synth_wav.py OUT.wav RATE CHANNELS BITS SECONDS [WASTED_BITS]

The signal is reproducible bit for bit (fixed-seed xorshift noise), so
regenerating the fixtures yields identical input for the encoders.
WASTED_BITS > 0 zeroes that many low bits of every sample, which makes
FLAC encoders use the "wasted bits" feature.
"""
import math
import struct
import sys


def main():
    out, rate, channels, bits, seconds = sys.argv[1:6]
    rate, channels, bits = int(rate), int(channels), int(bits)
    wasted = int(sys.argv[6]) if len(sys.argv) > 6 else 0
    frames = int(float(seconds) * rate)
    full = (1 << (bits - 1)) - 1
    x = 0x9E3779B97F4A7C15
    data = bytearray()
    bps = (bits + 7) // 8
    for n in range(frames):
        for c in range(channels):
            x ^= (x << 13) & 0xFFFFFFFFFFFFFFFF
            x ^= x >> 7
            x ^= (x << 17) & 0xFFFFFFFFFFFFFFFF
            noise = ((x >> 40) % 512) - 256  # about 8 bits of noise
            tone = math.sin(2 * math.pi * (220 * (c + 1)) * n / rate)
            v = int(0.5 * full * tone) + noise * max(1, full >> 15)
            v = max(-full - 1, min(full, v))
            v = (v >> wasted) << wasted
            if bits == 8:
                data += struct.pack("<B", v + 128)  # 8-bit WAV is unsigned
            else:
                data += v.to_bytes(bps, "little", signed=True)
    fmt = struct.pack("<HHIIHH", 1, channels, rate, rate * channels * bps,
                      channels * bps, bits)
    with open(out, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", 4 + 8 + len(fmt) + 8 + len(data))
                + b"WAVE" + b"fmt " + struct.pack("<I", len(fmt)) + fmt
                + b"data" + struct.pack("<I", len(data)) + data)


if __name__ == "__main__":
    main()

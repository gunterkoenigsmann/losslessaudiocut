#!/usr/bin/env python3
"""Inserts 700 bytes of junk after the 20th frame of an MP3 file.

Usage: mp3_damage.py IN OUT

The junk contains bytes that look like frame sync words, so a scanner
that accepts a sync word without checking the following frame would be
misled.
"""
import sys


def frame_len(h):
    """Length of an MPEG-1 Layer III frame (only what the fixtures use)."""
    bitrates = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320]
    rates = [44100, 48000, 32000]
    return 144 * bitrates[h[2] >> 4] * 1000 // rates[(h[2] >> 2) & 3] + ((h[2] >> 1) & 1)


def main():
    src, dst = sys.argv[1:3]
    data = open(src, "rb").read()
    pos = 0
    for _ in range(20):
        pos += frame_len(data[pos:pos + 4])
    junk = bytes((i * 37) & 0xFF for i in range(700))
    junk = junk[:100] + b"\xff\xfb\x90\x64" + junk[104:]
    open(dst, "wb").write(data[:pos] + junk + data[pos:])


if __name__ == "__main__":
    main()

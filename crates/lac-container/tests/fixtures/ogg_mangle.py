#!/usr/bin/env python3
"""Damages Ogg files the way real-world recordings are damaged.

Usage:
  ogg_mangle.py granules IN OUT   replace every granule position of audio
                                  pages with a pseudo-random 40-bit value (as some
                                  radio stations do) and fix the CRCs
  ogg_mangle.py damage IN OUT N   flip one byte in the body of page N, so
                                  its CRC no longer matches

The pseudo-random sequence is fixed, so the output is reproducible.
"""
import struct
import sys


def crc32(data):
    """Ogg CRC-32: polynomial 0x04C11DB7, MSB first, init 0, no final XOR."""
    crc = 0
    for b in data:
        crc ^= b << 24
        for _ in range(8):
            crc = ((crc << 1) ^ 0x04C11DB7) if crc & 0x80000000 else crc << 1
            crc &= 0xFFFFFFFF
    return crc


def pages(data):
    """Yields (offset, length) of every page."""
    pos = 0
    while pos < len(data):
        assert data[pos:pos + 4] == b"OggS", f"no page at {pos}"
        n = data[pos + 26]
        body = sum(data[pos + 27:pos + 27 + n])
        yield pos, 27 + n + body
        pos += 27 + n + body


def main():
    mode, src, dst = sys.argv[1:4]
    data = bytearray(open(src, "rb").read())
    if mode == "granules":
        x = 0x2545F4914F6CDD1D
        for i, (pos, length) in enumerate(pages(data)):
            granule = struct.unpack_from("<q", data, pos + 6)[0]
            # Leave header pages (granule 0) and "no packet ends" (-1) alone.
            if granule <= 0:
                continue
            x ^= (x << 13) & 0xFFFFFFFFFFFFFFFF
            x ^= x >> 7
            x ^= (x << 17) & 0xFFFFFFFFFFFFFFFF
            struct.pack_into("<q", data, pos + 6, x & 0xFFFFFFFFFF)
            struct.pack_into("<I", data, pos + 22, 0)
            crc = crc32(data[pos:pos + length])
            struct.pack_into("<I", data, pos + 22, crc)
    elif mode == "damage":
        n = int(sys.argv[4])
        pos, length = list(pages(data))[n]
        data[pos + length // 2] ^= 0x5A
    else:
        sys.exit(f"unknown mode {mode}")
    open(dst, "wb").write(data)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Rewrites Matroska files the way live recorders write them.

Usage:
  mkv_mangle.py unknown-clusters IN OUT   give every Cluster an unknown size
                                          (same field length, so nothing moves)
  mkv_mangle.py damage IN OUT N           zero the ID of the first block in
                                          cluster N (0x00 can never start an
                                          element header), as a network
                                          dropout leaves the file

Browsers (MediaRecorder) stream WebM without seeking back, so their
clusters have the reserved "unknown" size (RFC 8794 6.2).
"""
import sys

CLUSTER = bytes.fromhex("1F43B675")


def vint_len(b):
    n = 1
    while n <= 8 and not b & (0x80 >> (n - 1)):
        n += 1
    return n


def clusters(data):
    """Yields (offset, size field offset, size field length, body size)."""
    pos = 0
    while True:
        pos = data.find(CLUSTER, pos)
        if pos < 0:
            return
        at = pos + 4
        n = vint_len(data[at])
        size = data[at] & (0xFF >> n)
        for b in data[at + 1:at + n]:
            size = size << 8 | b
        yield pos, at, n, size
        pos = at + n + size


def main():
    mode, src, dst = sys.argv[1:4]
    data = bytearray(open(src, "rb").read())
    if mode == "unknown-clusters":
        for _, at, n, _ in list(clusters(data)):
            # All value bits set: 0xFF for one byte, 0x7FFF for two, ...
            data[at:at + n] = ((1 << (7 * n)) - 1 | 1 << (7 * n)).to_bytes(n, "big")
    elif mode == "damage":
        _, at, n, _ = list(clusters(data))[int(sys.argv[4])]
        # The cluster body starts with its Timestamp element (ID 0xE7, a
        # one-byte size); the first block follows it.
        body = at + n
        assert data[body] == 0xE7, "cluster does not start with a Timestamp"
        first_block = body + 2 + (data[body + 1] & 0x7F)
        data[first_block] = 0
    else:
        sys.exit(f"unknown mode {mode}")
    open(dst, "wb").write(data)


if __name__ == "__main__":
    main()

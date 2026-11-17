#!/usr/bin/env python3
"""Decides the version of a Python package build and writes it into
crates/lac-python/pyproject.toml.

    pypi_version.py v1.2.0            -> 1.2.0
    pypi_version.py v1.2.0-rc1        -> 1.2.0rc1
    pypi_version.py v1.2.0 --post     -> 1.2.0, or 1.2.0.postN if 1.2.0 is
                                         already on PyPI
    ... --write crates/lac-python/pyproject.toml

A tag build publishes the tag's version. A rebuild that only bundles a
newer FFmpeg (the weekly ffmpeg-update.yml) publishes the same code as a
post-release (PEP 440), the next one free on PyPI: PyPI never accepts a
file twice, and `pip install -U` picks a post-release over its base.

Standard library only, so it runs on a bare runner.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

PYPI_JSON = "https://pypi.org/pypi/losslessaudiocut/json"


def pep440(tag: str) -> str:
    """The PEP 440 form of a release tag or Cargo version.

    ``v1.2.0`` -> ``1.2.0``; the pre-release suffixes Cargo versions use
    (``-rc1``, ``-beta.2``, ``-alpha1``) become ``rc1``, ``b2``, ``a1``.
    A version that already is in PEP 440 form (``1.2.0.post1``, as the
    build jobs get it from the plan) is returned unchanged.
    """
    if re.fullmatch(r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+)?(?:\.post\d+)?", tag):
        return tag
    m = re.fullmatch(r"v?(\d+\.\d+\.\d+)(?:-(alpha|a|beta|b|rc)\.?(\d+))?", tag.strip())
    if not m:
        raise ValueError(f"not a release version: {tag!r}")
    base, kind, n = m.groups()
    if kind is None:
        return base
    short = {"alpha": "a", "beta": "b"}.get(kind, kind)
    return f"{base}{short}{n}"


def next_post(base: str, released: set[str]) -> str:
    """``base`` if it is not released yet, else its next free post-release."""
    if base not in released:
        return base
    pattern = re.compile(re.escape(base) + r"\.post(\d+)")
    posts = [int(m.group(1)) for v in released if (m := pattern.fullmatch(v))]
    return f"{base}.post{max(posts, default=0) + 1}"


def released_versions(url: str = PYPI_JSON) -> set[str]:
    """The versions of the package on PyPI; empty before the first upload."""
    try:
        with urllib.request.urlopen(url, timeout=30) as r:
            return set(json.load(r)["releases"])
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return set()
        raise


def write_version(pyproject: Path, version: str) -> None:
    """Replaces the dynamic version (from Cargo.toml) by a fixed one."""
    text = pyproject.read_text(encoding="utf-8")
    new, n = re.subn(r'(?m)^dynamic = \["version"\]$', f'version = "{version}"', text)
    if n != 1:
        raise ValueError(f'{pyproject}: expected one line dynamic = ["version"]')
    pyproject.write_text(new, encoding="utf-8")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("tag", help="release tag (v1.2.0) or Cargo version")
    ap.add_argument("--post", action="store_true", help="next post-release if the version is on PyPI")
    ap.add_argument("--write", type=Path, metavar="PYPROJECT", help="set the version in this pyproject.toml")
    args = ap.parse_args()
    version = pep440(args.tag)
    if args.post:
        version = next_post(version, released_versions())
    if args.write:
        write_version(args.write, version)
    print(version)
    return 0


if __name__ == "__main__":
    sys.exit(main())

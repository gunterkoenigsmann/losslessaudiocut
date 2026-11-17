#!/usr/bin/env bash
# Puts the license files into the Python package before `maturin build`:
#
#   - the project's LICENSE next to pyproject.toml, where maturin finds it
#     for the wheel's .dist-info,
#   - the bundled FFmpeg's license and source note (the directory
#     build-ffmpeg.sh writes to PREFIX/share/doc/ffmpeg, or the license
#     files of a prebuilt FFmpeg) into losslessaudiocut/ffmpeg/.
#
# Usage: stage-wheel.sh FFMPEG_DOC_DIR
#
# Both copies are build output and ignored by git.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
doc="${1:?usage: stage-wheel.sh FFMPEG_DOC_DIR}"
pkg="$root/crates/lac-python"

cp "$root/LICENSE" "$pkg/LICENSE"
rm -rf "$pkg/python/losslessaudiocut/ffmpeg"
mkdir -p "$pkg/python/losslessaudiocut/ffmpeg"
cp -R "$doc"/. "$pkg/python/losslessaudiocut/ffmpeg/"
ls -l "$pkg/python/losslessaudiocut/ffmpeg"

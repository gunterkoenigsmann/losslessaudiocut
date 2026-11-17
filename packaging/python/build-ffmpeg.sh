#!/usr/bin/env bash
# Builds the FFmpeg that the Python wheels link statically: libavcodec with
# only the audio decoders losslessaudiocut uses, plus libavformat,
# libswresample (the Opus decoder needs it) and libavutil. Everything else
# (programs, demuxers, muxers, encoders, filters, network, hardware
# acceleration, external libraries) is disabled, which keeps the wheels small and leaves out the
# parts of FFmpeg where most security advisories land.
#
# Usage: build-ffmpeg.sh PREFIX [VERSION]
#
#   PREFIX   where to install (include/, lib/, lib/pkgconfig/); point
#            FFMPEG_DIR at it when building the wheel.
#   VERSION  the FFmpeg release, e.g. 9.0.2. Defaults to $FFMPEG_VERSION,
#            else the one in packaging/python/ffmpeg-version (the weekly
#            ffmpeg-update.yml workflow keeps that file at the newest point
#            release).
#
# The source is the release tag nVERSION of FFmpeg's git repository (the
# GitHub mirror is the fastest to clone). Set FFMPEG_SRC to an existing
# source tree to skip the download.
#
# Linux and macOS only; the Windows wheels use a prebuilt LGPL build, see
# .github/workflows/python.yml.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
prefix="${1:?usage: build-ffmpeg.sh PREFIX [VERSION]}"
version="${2:-${FFMPEG_VERSION:-$(tr -d '[:space:]' < "$here/ffmpeg-version")}}"
mkdir -p "$prefix"
prefix="$(cd "$prefix" && pwd)"

if [ -n "${FFMPEG_SRC:-}" ]; then
  src="$FFMPEG_SRC"
else
  src="$(mktemp -d)/ffmpeg"
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "n$version" \
    https://github.com/FFmpeg/FFmpeg.git "$src"
fi

# The decoders behind lac-decode's codec_id() (crates/lac-decode/src/ffi.rs).
# MP3, AAC and AC-3 come as float and fixed-point decoders; the float ones
# are what FFmpeg picks for the codec id, the fixed ones are kept so that a
# preference change in FFmpeg cannot leave us without a decoder. No video
# decoders: the Python API has no video preview.
decoders=(
  flac vorbis opus speex
  mp3 mp3float aac aac_fixed alac ac3 ac3_fixed amrnb amrwb
  pcm_s8 pcm_u8 pcm_s16le pcm_s16be pcm_s24le pcm_s24be pcm_s32le pcm_s32be
  pcm_f32le pcm_f32be pcm_f64le pcm_f64be pcm_alaw pcm_mulaw
)

flags=(
  --prefix="$prefix"
  --enable-static --disable-shared --enable-pic
  # LGPL only (no --enable-gpl / --enable-nonfree): the wheel stays
  # distributable under the project's GPL-3.0-or-later.
  --disable-programs --disable-doc
  --disable-network --disable-autodetect --disable-everything
  # libswresample stays: FFmpeg's Opus decoder resamples with it.
  --disable-avdevice --disable-avfilter --disable-swscale
  --enable-decoder="$(IFS=,; echo "${decoders[*]}")"
)
# FFmpeg's x86 assembly needs nasm. Without it the decoders still work,
# only slower; say so instead of failing.
case "$(uname -m)" in
  x86_64 | i?86)
    if ! command -v nasm > /dev/null; then
      echo "warning: nasm not found, building without x86 assembly" >&2
      flags+=(--disable-x86asm)
    fi
    ;;
esac

build="$(mktemp -d)"
cd "$build"
"$src/configure" "${flags[@]}" ${FFMPEG_CONFIGURE_EXTRA:-}
jobs="$(getconf _NPROCESSORS_ONLN 2> /dev/null || sysctl -n hw.ncpu 2> /dev/null || echo 2)"
make -j"$jobs"
make install

# What the LGPL asks of a binary distribution: the license text and where
# to get the source. stage-wheel.sh copies this directory into the wheel.
doc="$prefix/share/doc/ffmpeg"
mkdir -p "$doc"
cp "$src/COPYING.LGPLv2.1" "$doc/COPYING.LGPLv2.1"
cat > "$doc/README.txt" << README
This package contains FFmpeg $version (libavcodec, libavformat,
libswresample, libavutil), licensed under the GNU Lesser General Public
License version 2.1 or later (COPYING.LGPLv2.1), linked statically into
the extension module.

Source: https://ffmpeg.org/releases/ffmpeg-$version.tar.xz
        (git: https://git.ffmpeg.org/ffmpeg.git, tag n$version)
Built unmodified by packaging/python/build-ffmpeg.sh of
https://github.com/gunterkoenigsmann/losslessaudiocut with:
  configure ${flags[*]#--prefix=*}
README
echo "FFmpeg $version installed to $prefix"

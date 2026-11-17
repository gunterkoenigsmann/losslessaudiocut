#!/usr/bin/env bash
# Builds the Linux wheel inside a manylinux_2_28 container (AlmaLinux 8,
# glibc 2.28), so that it runs on every Linux distribution from about 2019
# on. Run from the repository root:
#
#   docker run --rm -v "$PWD":/io -w /io quay.io/pypa/manylinux_2_28_x86_64 \
#     packaging/python/build-linux-wheel.sh dist
#
# (aarch64: the image manylinux_2_28_aarch64 on an arm64 machine.)
# It installs Rust, nasm and libclang (bindgen) in the container, builds
# the minimal static FFmpeg (build-ffmpeg.sh), stages the license files and
# lets maturin build and check the wheel (no library outside the manylinux
# set may be linked). The wheel lands in OUTDIR, owned by the user who owns
# the repository checkout.
set -euo pipefail

out="${1:?usage: build-linux-wheel.sh OUTDIR}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

# nasm: FFmpeg's x86 assembly. clang: libclang for ffmpeg-sys-next's
# bindgen. git: fetching the FFmpeg source.
dnf install -y -q nasm clang git > /dev/null || dnf install -y -q clang git > /dev/null

if ! command -v cargo > /dev/null; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
fi

ffmpeg="/tmp/ffmpeg"
"$here/build-ffmpeg.sh" "$ffmpeg"
"$here/stage-wheel.sh" "$ffmpeg/share/doc/ffmpeg"

# Any of the container's Pythons can run maturin; the wheel is abi3.
python=/opt/python/cp312-cp312/bin/python
"$python" -m pip install -q "maturin>=1.9,<2"
cd "$root/crates/lac-python"
FFMPEG_DIR="$ffmpeg" "$python" -m maturin build --release --locked \
  --compatibility manylinux_2_28 --out "$root/$out"

# Files created as root in the container belong to the checkout's owner.
chown -R "$(stat -c %u:%g "$root")" "$root/$out" "$root/target" 2> /dev/null || true

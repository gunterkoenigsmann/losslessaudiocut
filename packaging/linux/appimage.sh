#!/usr/bin/env bash
# Packs the release binaries into an AppImage for the architecture of the
# machine it runs on (x86_64 or aarch64):
#
#   packaging/linux/appimage.sh VERSION OUTDIR
#
# Expects `cargo build --release -p lac-gui -p lac-cli` to have run. Writes
# OUTDIR/losslessaudiocut-VERSION-linux-ARCH.AppImage.
#
# The AppImage starts the GUI. The command line tool `lac` is inside as well;
# `./losslessaudiocut-*.AppImage --appimage-extract` unpacks it to
# squashfs-root/usr/bin/lac, which finds the bundled libraries by its rpath.
#
# Tools: linuxdeploy (copies every library the binaries need into the
# AppDir, except the ones its excludelist says must come from the host, such
# as glibc, libGL and libasound) and its GTK plugin (GTK 3 modules, themes,
# GSettings schemas and the environment to find them). Both are downloaded
# from their "continuous" releases.
set -euo pipefail

version="$1"
outdir="$(mkdir -p "$2" && cd "$2" && pwd)"
root="$(cd "$(dirname "$0")/../.." && pwd)"
arch="$(uname -m)"            # x86_64 or aarch64, as linuxdeploy names them
target_dir="${CARGO_TARGET_DIR:-$root/target}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# --- Tools -----------------------------------------------------------------
tools="$work/tools"
mkdir -p "$tools"
curl -sSfL -o "$tools/linuxdeploy" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$arch.AppImage"
curl -sSfL -o "$tools/linuxdeploy-plugin-gtk.sh" \
    "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh"
chmod +x "$tools"/*
# For the log: which tool versions built this package.
sha256sum "$tools"/*
# CI runners (and containers) have no FUSE; AppImages can unpack themselves
# to a temporary directory instead. This covers linuxdeploy and the
# appimagetool inside it.
export APPIMAGE_EXTRACT_AND_RUN=1

# --- AppDir ----------------------------------------------------------------
appdir="$work/AppDir"
icons="$appdir/usr/share/icons/hicolor"
mkdir -p "$icons/scalable/apps" "$icons/256x256/apps"
cp "$root/packaging/icons/losslessaudiocut.svg" "$icons/scalable/apps/"
# A PNG as well: some desktops and AppImage integrators only use bitmaps.
rsvg-convert -w 256 -h 256 "$root/packaging/icons/losslessaudiocut.svg" \
    -o "$icons/256x256/apps/losslessaudiocut.png"

# The binaries' rpath is set by linuxdeploy so that they find usr/lib.
# DEPLOY_GTK_VERSION: the GTK plugin cannot tell GTK 3 from GTK 4 by itself.
export DEPLOY_GTK_VERSION=3
# The AppImage's file name; linuxdeploy's appimage plugin reads these.
export LDAI_OUTPUT="$outdir/losslessaudiocut-$version-linux-$arch.AppImage"
export LINUXDEPLOY_OUTPUT_VERSION="$version"
# linuxdeploy finds its plugins (linuxdeploy-plugin-*) on PATH.
export PATH="$tools:$PATH"

linuxdeploy \
    --appdir "$appdir" \
    --executable "$target_dir/release/losslessaudiocut" \
    --executable "$target_dir/release/lac" \
    --desktop-file "$root/packaging/linux/losslessaudiocut.desktop" \
    --icon-file "$icons/256x256/apps/losslessaudiocut.png" \
    --plugin gtk \
    --output appimage

ls -l "$LDAI_OUTPUT"

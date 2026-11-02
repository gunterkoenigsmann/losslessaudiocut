#!/bin/sh
# Regenerates the committed test fixtures. Requires python3, flac and ffmpeg.
# Run from anywhere; output goes next to this script.
set -eu
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
synth="python3 synth_wav.py"

# FLAC, encoded by the reference encoder (libFLAC).
$synth "$tmp/a.wav" 44100 2 16 1.5
flac -s -f -8 -o flac_stereo16_44k.flac "$tmp/a.wav"   # LPC, mid/side, seektable
$synth "$tmp/b.wav" 48000 4 24 0.4
flac -s -f -5 --channel-map=none -o flac_quad24_48k.flac "$tmp/b.wav"
$synth "$tmp/c.wav" 8000 1 8 1.0
flac -s -f -0 -o flac_mono8_8k_fixed.flac "$tmp/c.wav"  # fixed predictors
$synth "$tmp/d.wav" 22050 2 16 1.0 3
flac -s -f -5 --blocksize=1152 -o flac_wasted_22k.flac "$tmp/d.wav"
metaflac --set-tag=TITLE=Fixture --set-tag=ARTIST=losslessaudiocut flac_wasted_22k.flac

# FLAC, encoded by ffmpeg, with the other stereo decorrelation modes.
ffmpeg -v error -y -i "$tmp/a.wav" -t 0.5 -ch_mode left_side flac_leftside_ffmpeg.flac
ffmpeg -v error -y -i "$tmp/a.wav" -t 0.5 -ch_mode right_side flac_rightside_ffmpeg.flac

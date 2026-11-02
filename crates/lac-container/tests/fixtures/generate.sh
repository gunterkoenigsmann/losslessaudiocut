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

# Ogg, encoded by ffmpeg with the reference codec libraries. "bitexact"
# keeps encoder version strings out so regeneration is reproducible;
# 100 ms pages give the tests many page boundaries; serial_offset gives the
# links of a chained file distinct serials.
og="ffmpeg -v error -y"
oo="-fflags +bitexact -page_duration 100000"
$synth "$tmp/v.wav" 44100 2 16 1.5
$og -i "$tmp/v.wav" $oo -c:a libvorbis -q:a 3 ogg_vorbis_stereo_44k.ogg
$synth "$tmp/m.wav" 22050 1 16 1.0
$og -i "$tmp/m.wav" $oo -c:a libvorbis -q:a 1 ogg_vorbis_mono_22k.ogg
$synth "$tmp/o.wav" 48000 2 16 1.5 0 speech
$og -i "$tmp/o.wav" $oo -c:a libopus -b:a 64k ogg_opus_stereo.opus
$synth "$tmp/s.wav" 16000 1 16 1.0 0 speech
$og -i "$tmp/s.wav" $oo -c:a libspeex ogg_speex_16k.spx

# Chained files, as written by stream rippers: two complete streams.
$og -i "$tmp/v.wav" $oo -t 0.5 -c:a libvorbis -q:a 3 "$tmp/v1.ogg"
$og -i "$tmp/v.wav" $oo -ss 0.7 -c:a libvorbis -q:a 3 -serial_offset 7 "$tmp/v2.ogg"
cat "$tmp/v1.ogg" "$tmp/v2.ogg" > ogg_vorbis_chained.ogg
$og -i "$tmp/o.wav" $oo -t 0.5 -c:a libopus -b:a 64k "$tmp/o1.opus"
$og -i "$tmp/o.wav" $oo -ss 0.7 -c:a libopus -b:a 64k -serial_offset 7 "$tmp/o2.opus"
cat "$tmp/o1.opus" "$tmp/o2.opus" > ogg_opus_chained.opus

# Radio-style damage: random granule positions, and a page with a bad CRC.
python3 ogg_mangle.py granules ogg_vorbis_stereo_44k.ogg ogg_vorbis_badgranule.ogg
python3 ogg_mangle.py granules ogg_opus_stereo.opus ogg_opus_badgranule.opus
python3 ogg_mangle.py damage ogg_vorbis_stereo_44k.ogg ogg_vorbis_damaged.ogg 5

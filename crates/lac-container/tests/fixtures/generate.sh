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

# MP3, encoded by LAME (reference encoder) and by ffmpeg's libmp3lame.
$synth "$tmp/p.wav" 44100 2 16 1.5
lame --quiet -b 128 "$tmp/p.wav" mp3_cbr128_44k.mp3            # Info + LAME tag
lame --quiet -V 2 "$tmp/p.wav" mp3_vbr_44k.mp3                 # Xing + LAME tag
$synth "$tmp/q.wav" 22050 1 16 1.0
lame --quiet -b 64 "$tmp/q.wav" mp3_mpeg2_mono_22k.mp3         # MPEG-2, 576 samples/frame
$synth "$tmp/u.wav" 8000 1 16 1.0
lame --quiet -b 16 "$tmp/u.wav" mp3_mpeg25_8k.mp3              # MPEG 2.5
# No Xing/LAME tag, CRC-protected frames, ID3v1 tag at the end.
lame --quiet -b 160 -t -p --id3v1-only --tt Fixture "$tmp/p.wav" mp3_notag_crc.mp3
# ffmpeg's encoder string "Lavc", and an ID3v2 tag in front.
$og -i "$tmp/p.wav" -fflags +bitexact -c:a libmp3lame -b:a 96k -metadata title=Fixture mp3_ffmpeg_id3v2.mp3
# Junk in the middle, as left by a broken download.
python3 mp3_damage.py mp3_cbr128_44k.mp3 mp3_junk.mp3

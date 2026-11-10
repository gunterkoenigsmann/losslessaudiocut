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

# Matroska/WebM with Opus, encoded by ffmpeg's libopus. 200 ms clusters
# give the tests many cluster boundaries; "-live 1" writes an unknown
# Segment size like a streaming recorder.
mo="-fflags +bitexact -cluster_time_limit 200"
$og -i "$tmp/o.wav" $mo -c:a libopus -b:a 64k mkv_opus_stereo.webm
$synth "$tmp/n.wav" 48000 1 16 1.0 0 speech
$og -i "$tmp/n.wav" $mo -c:a libopus -b:a 32k -frame_duration 10 mkv_opus_mono_10ms.mka
$og -i "$tmp/o.wav" $mo -live 1 -c:a libopus -b:a 64k "$tmp/live.webm"
python3 mkv_mangle.py unknown-clusters "$tmp/live.webm" mkv_opus_live.webm
python3 mkv_mangle.py damage mkv_opus_stereo.webm mkv_opus_damaged.webm 3
# A video track in front of the audio, as OBS or a phone camera writes it.
$og -f lavfi -i "color=c=gray:s=16x16:r=5:d=1" -i "$tmp/n.wav" $mo \
    -c:v mjpeg -c:a libopus -b:a 32k mkv_opus_with_video.mkv

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

# AIFF and AIFF-C, written by ffmpeg: big-endian 16 and 24 bit with tags
# in text chunks and an ID3 chunk, and the AIFF-C types sowt (little-endian)
# and fl32 (float).
$synth "$tmp/ai.wav" 44100 2 16 0.5
$og -i "$tmp/ai.wav" -fflags +bitexact -c:a pcm_s16be -metadata title=Fixture \
    -metadata artist=losslessaudiocut -write_id3v2 1 aiff_s16_44k.aiff
$synth "$tmp/aj.wav" 48000 1 24 0.4
$og -i "$tmp/aj.wav" -fflags +bitexact -c:a pcm_s24be aiff_s24_48k_mono.aif
$synth "$tmp/ak.wav" 22050 2 16 0.4
$og -i "$tmp/ak.wav" -fflags +bitexact -c:a pcm_s16le -f aiff aifc_sowt_22k.aifc
$og -i "$tmp/ak.wav" -fflags +bitexact -c:a pcm_f32be -f aiff aifc_fl32_22k.aifc

# MP4/M4A with AAC, encoded by ffmpeg's native AAC encoder (AAC-LC): with
# tags and moov after mdat (ffmpeg's default); speech-like mono with moov
# first (+faststart); without an edit list, so the priming samples are
# part of the timeline; and with a video track in front of the audio, as
# a phone camera writes it. Perceptual noise substitution is off ("-aac_pns
# 0"): its noise comes from a random generator that runs from the start of
# the stream, so a cut decodes it differently (see tests/mp4.rs). The last
# file keeps it on to test exactly that.
ma="-fflags +bitexact -c:a aac -aac_pns 0"
$synth "$tmp/ma.wav" 44100 2 16 1.5
$og -i "$tmp/ma.wav" $ma -b:a 128k -metadata title=Fixture \
    -metadata artist=losslessaudiocut mp4_aac_stereo_44k.m4a
$synth "$tmp/mb.wav" 48000 1 16 1.0 0 speech
$og -i "$tmp/mb.wav" $ma -b:a 64k -movflags +faststart mp4_aac_mono_48k.m4a
$synth "$tmp/mc.wav" 22050 2 16 1.0
$og -i "$tmp/mc.wav" $ma -b:a 64k -use_editlist 0 mp4_aac_noedit_22k.m4a
$og -f lavfi -i "color=c=gray:s=16x16:r=5:d=1" -i "$tmp/mb.wav" -fflags +bitexact \
    -c:v mjpeg $ma -b:a 64k mp4_aac_with_video.mp4
$og -i "$tmp/ma.wav" -fflags +bitexact -c:a aac -b:a 96k mp4_aac_pns_44k.m4a
# Mono with PNS but without temporal noise shaping, for the transform-domain
# check of what a cut changes (lac-decode/tests/pns.rs).
$synth "$tmp/md.wav" 44100 1 16 1.5
$og -i "$tmp/md.wav" -fflags +bitexact -c:a aac -aac_tns 0 -b:a 64k mp4_aac_pns_mono_44k.m4a

# MP4/M4A with Apple Lossless (ALAC), encoded by ffmpeg: 16-bit stereo
# (4096-sample frames, the last one shorter) and 24-bit mono.
$synth "$tmp/la.wav" 44100 2 16 0.75
$og -i "$tmp/la.wav" -fflags +bitexact -c:a alac mp4_alac_stereo16_44k.m4a
$synth "$tmp/lb.wav" 48000 1 24 0.5
$og -i "$tmp/lb.wav" -fflags +bitexact -c:a alac mp4_alac_mono24_48k.m4a
# 24-bit stereo with correlated channels at the highest compression level:
# stereo mixing and the low byte of each sample stored apart ("shifted
# bytes") together.
$synth "$tmp/lc.wav" 44100 2 24 0.4
$og -i "$tmp/lc.wav" -af "pan=stereo|c0=0.8*c0+0.2*c1|c1=0.6*c0+0.4*c1" \
    -fflags +bitexact -c:a alac -compression_level 2 mp4_alac_stereo24_44k.m4a

# Fragmented MP4, as recorders write it so that an interrupted recording
# stays readable: AAC with an empty moov and 200 ms fragments whose data
# offsets count from the moof box (no edit list, so the priming samples
# are part of the timeline), and ALAC whose first samples are in moov's
# sample table and the rest in 150 ms fragments with explicit base
# offsets.
$og -i "$tmp/ma.wav" $ma -b:a 128k \
    -movflags frag_keyframe+empty_moov+default_base_moof -frag_duration 200000 \
    mp4_aac_fragmented_44k.m4a
$og -i "$tmp/lb.wav" -fflags +bitexact -c:a alac -movflags frag_keyframe \
    -frag_duration 150000 mp4_alac_fragmented_48k.m4a

# Raw AC-3, encoded by ffmpeg's native (fixed-point) AC-3 encoder: stereo
# 2/0 at 44.1 kHz (frame sizes alternate between 834 and 836 bytes),
# 5.1 (3/2 + LFE) at 48 kHz and mono at 32 kHz.
$synth "$tmp/ca.wav" 44100 2 16 1.0
$og -i "$tmp/ca.wav" -fflags +bitexact -c:a ac3_fixed -b:a 192k ac3_stereo_44k.ac3
$synth "$tmp/cb.wav" 48000 6 16 0.5
$og -i "$tmp/cb.wav" -fflags +bitexact -c:a ac3_fixed -b:a 384k ac3_51_48k.ac3
$synth "$tmp/cc.wav" 32000 1 16 0.5 0 speech
$og -i "$tmp/cc.wav" -fflags +bitexact -c:a ac3_fixed -b:a 64k ac3_mono_32k.ac3

# Raw AAC in ADTS (.aac), encoded by ffmpeg's native AAC encoder: stereo at
# 44.1 kHz and mono speech at 22.05 kHz without perceptual noise
# substitution (bit-exact cuts), and one with it (see tests/adts.rs).
$synth "$tmp/aa.wav" 44100 2 16 1.5
$og -i "$tmp/aa.wav" $ma -b:a 128k -f adts adts_aac_stereo_44k.aac
$synth "$tmp/ab.wav" 22050 1 16 1.0 0 speech
$og -i "$tmp/ab.wav" $ma -b:a 48k -f adts adts_aac_mono_22k.aac
$og -i "$tmp/aa.wav" -fflags +bitexact -c:a aac -b:a 96k -f adts adts_aac_pns_44k.aac

# Phone-like videos with AAC audio: 64x64 pixels, 2 s at 30 fps, a
# keyframe every 15 frames (phones write one about every second), in each
# video codec phones record with. "bitexact" keeps encoder version strings
# out of the containers; x264/x265 still write their version into an SEI.
#
# - H.264 without B-frames, the common Android case;
# - H.264 with B-frames and an open GOP (non-IDR I-frames that the muxer
#   marks as sync samples) and a display rotation (90 degrees counter-
#   clockwise in ffmpeg's terms, set when remuxing), as a portrait video;
# - H.264 fragmented (sync flags and composition offsets in trun boxes);
# - HEVC in QuickTime .mov with B-frames and an open GOP (CRA with RASL
#   pictures), as iPhones and Android "high efficiency" record it;
# - AV1 (SVT-AV1) and VP9, as newer Android phones record;
# - ProRes in .mov (every frame a keyframe, no stss), as iPhone Pro;
# - MPEG-4 Part 2 in .3gp, as old phones.
$synth "$tmp/va.wav" 48000 2 16 2.0
vs="-f lavfi -i testsrc2=s=64x64:r=30:d=2"
vo="-fflags +bitexact -c:a aac -aac_pns 0 -b:a 64k"
$og $vs -i "$tmp/va.wav" $vo -c:v libx264 -bf 0 -g 15 -keyint_min 15 \
    -sc_threshold 0 -pix_fmt yuv420p video_h264_aac.mp4
$og $vs -i "$tmp/va.wav" $vo -c:v libx264 -bf 2 -g 15 -keyint_min 15 \
    -sc_threshold 0 -x264-params open-gop=1 -pix_fmt yuv420p \
    "$tmp/rot.mp4"
$og -display_rotation 90 -i "$tmp/rot.mp4" -c copy -fflags +bitexact \
    video_h264_opengop_rot90.mp4
$og $vs -i "$tmp/va.wav" $vo -c:v libx264 -bf 2 -g 15 -keyint_min 15 \
    -sc_threshold 0 -pix_fmt yuv420p \
    -movflags frag_keyframe+empty_moov+default_base_moof video_h264_fragmented.mp4
$og $vs -i "$tmp/va.wav" $vo -c:v libx265 -tag:v hvc1 -pix_fmt yuv420p \
    -x265-params log-level=error:keyint=15:min-keyint=15:scenecut=0:bframes=2:open-gop=1 \
    video_hevc_opengop.mov
$og $vs -i "$tmp/va.wav" $vo -c:v libsvtav1 -g 15 -pix_fmt yuv420p \
    -svtav1-params scd=0 video_av1.mp4
$og $vs -i "$tmp/va.wav" $vo -c:v libvpx-vp9 -g 15 -keyint_min 15 -b:v 100k \
    -pix_fmt yuv420p video_vp9.mp4
$og $vs -i "$tmp/va.wav" $vo -c:v prores_ks -profile:v 0 video_prores.mov
$og $vs -i "$tmp/va.wav" $vo -c:v mpeg4 -g 15 -bf 0 video_mpeg4.3gp

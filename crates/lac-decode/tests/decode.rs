//! Decoding tests against the committed fixtures of `lac-container`.
//!
//! The oracle is the ffmpeg command line tool: it demuxes with *its own*
//! demuxers, so agreement shows that our packets, codec headers and
//! timeline (encoder delay, pre-skip, end trim) are what a player sees.
//! Both sides use libavcodec's decoders, so the samples must match bit for
//! bit.

use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::process::Command;

use lac_decode::{Flow, decode_all, decode_to_vec};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lac-container/tests/fixtures")
        .join(name)
}

/// Decodes `name` with our demuxer and libavcodec.
fn ours(name: &str) -> (Vec<f32>, lac_decode::DecodeReport, u64, u16) {
    let mut r = BufReader::new(File::open(fixture_path(name)).expect("fixture"));
    let c = lac_container::open(&mut r).expect("opens");
    let total = c.index().total_samples();
    let channels = c.index().info().channels;
    let (pcm, report) = decode_to_vec(c.as_ref(), &mut r).expect("decodes");
    (pcm, report, total, channels)
}

/// Decodes `name` with the ffmpeg tool to interleaved f32, or `None` if
/// ffmpeg is not installed (fatal when `LAC_REQUIRE_TOOLS` is set, as in CI).
fn ffmpeg(name: &str) -> Option<Vec<f32>> {
    ffmpeg_file(&fixture_path(name))
}

/// Decodes the file at `path` like [`ffmpeg`].
fn ffmpeg_file(path: &Path) -> Option<Vec<f32>> {
    let name = path.display();
    // AC-3: seed the dither generator per frame, as lac-decode does (see
    // lac_container::ac3); the default stream-wide generator gives
    // different noise to every decoder that starts elsewhere.
    let ac3 = path.extension().is_some_and(|e| e == "ac3");
    let opts: &[&str] = if ac3 { &["-cons_noisegen", "1"] } else { &[] };
    let out = match Command::new("ffmpeg")
        .args(["-v", "error"])
        .args(opts)
        .arg("-i")
        .arg(path)
        .args(["-f", "f32le", "-"])
        .output()
    {
        Ok(o) => o,
        Err(_) if std::env::var_os("LAC_REQUIRE_TOOLS").is_some() => {
            panic!("ffmpeg is required (LAC_REQUIRE_TOOLS is set) but not installed")
        }
        Err(_) => {
            eprintln!("ffmpeg not installed, skipping cross-check");
            return None;
        }
    };
    assert!(out.status.success(), "ffmpeg failed on {name}");
    Some(
        out.stdout
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&b| f32::from_le_bytes(b))
            .collect(),
    )
}

/// Fixtures whose timeline ffmpeg reproduces exactly.
const EXACT: [&str; 31] = [
    "adts_aac_stereo_44k.aac",
    "adts_aac_mono_22k.aac",
    "ac3_stereo_44k.ac3",
    "ac3_51_48k.ac3",
    "ac3_mono_32k.ac3",
    "mp4_alac_stereo16_44k.m4a",
    "mp4_alac_mono24_48k.m4a",
    "mp4_alac_fragmented_48k.m4a",
    "mp4_alac_stereo24_44k.m4a",
    "aiff_s16_44k.aiff",
    "aiff_s24_48k_mono.aif",
    "aifc_sowt_22k.aifc",
    "aifc_fl32_22k.aifc",
    "flac_stereo16_44k.flac",
    "flac_mono8_8k_fixed.flac",
    "flac_quad24_48k.flac",
    "flac_wasted_22k.flac",
    "flac_leftside_ffmpeg.flac",
    "flac_rightside_ffmpeg.flac",
    "ogg_vorbis_stereo_44k.ogg",
    "ogg_vorbis_mono_22k.ogg",
    "ogg_opus_stereo.opus",
    "ogg_speex_16k.spx",
    "mp3_cbr128_44k.mp3",
    "mp3_vbr_44k.mp3",
    "mp3_mpeg2_mono_22k.mp3",
    "mp3_mpeg25_8k.mp3",
    "mp3_ffmpeg_id3v2.mp3",
    "mkv_opus_stereo.webm",
    "mkv_opus_mono_10ms.mka",
    "mkv_opus_live.webm",
];

#[test]
fn decodes_like_ffmpeg_bit_for_bit() {
    for name in EXACT {
        let (pcm, report, total, channels) = ours(name);
        assert!(report.warnings.is_empty(), "{name}: {:?}", report.warnings);
        assert_eq!(report.frames, total, "{name}: frames reported");
        assert_eq!(
            pcm.len() as u64,
            total * u64::from(channels),
            "{name}: length"
        );
        let Some(want) = ffmpeg(name) else { return };
        assert_bit_identical(name, &pcm, &want, channels);
    }
}

/// Panics unless `pcm` and `want` hold the same samples, bit for bit.
fn assert_bit_identical(name: &str, pcm: &[f32], want: &[f32], channels: u16) {
    assert_eq!(pcm.len(), want.len(), "{name}: length differs from ffmpeg");
    if let Some(i) = (0..pcm.len()).find(|&i| pcm[i].to_bits() != want[i].to_bits()) {
        panic!(
            "{name}: sample {} channel {} is {} but ffmpeg says {}",
            i / usize::from(channels),
            i % usize::from(channels),
            pcm[i],
            want[i]
        );
    }
}

/// Splits an Ogg file into its chain links: a new link starts at every
/// BOS page that follows a non-BOS page (RFC 3533 4: grouped streams put
/// all their BOS pages first, so those stay together).
fn split_chain(bytes: &[u8]) -> Vec<&[u8]> {
    let mut links = Vec::new();
    let (mut pos, mut start, mut after_bos) = (0usize, 0usize, true);
    while pos + 27 <= bytes.len() {
        assert_eq!(&bytes[pos..pos + 4], b"OggS", "page at byte {pos}");
        let bos = bytes[pos + 5] & 0x02 != 0;
        if bos && !after_bos {
            links.push(&bytes[start..pos]);
            start = pos;
        }
        after_bos = bos;
        // RFC 3533 6: 27 header bytes, the lacing values, then the body.
        let n = usize::from(bytes[pos + 26]);
        let body: usize = bytes[pos + 27..pos + 27 + n]
            .iter()
            .map(|&l| usize::from(l))
            .sum();
        pos += 27 + n + body;
    }
    links.push(&bytes[start..]);
    links
}

/// A chained Ogg file decodes, bit for bit, to the concatenation of its
/// links decoded as separate files by ffmpeg: every link starts with a
/// fresh decoder, and the encoder padding at the end of each link (its end
/// trim) is dropped, also in the middle of the file. ffmpeg does not reset
/// its Vorbis decoder at a chain boundary when it reads the chained file
/// itself, so the links are split apart for it.
#[test]
fn chained_files_decode_like_their_links() {
    for name in ["ogg_vorbis_chained.ogg", "ogg_opus_chained.opus"] {
        let (pcm, report, total, channels) = ours(name);
        assert!(report.warnings.is_empty(), "{name}: {:?}", report.warnings);
        assert_eq!(pcm.len() as u64, total * u64::from(channels), "{name}");
        let bytes = std::fs::read(fixture_path(name)).expect("fixture");
        let links = split_chain(&bytes);
        assert_eq!(links.len(), 2, "{name}");
        let dir = std::env::temp_dir().join(format!("lac-decode-chain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut want = Vec::new();
        for (i, link) in links.iter().enumerate() {
            let path = dir.join(format!("{i}-{name}"));
            std::fs::write(&path, link).expect("write link");
            let Some(d) = ffmpeg_file(&path) else { return };
            want.extend(d);
        }
        assert_bit_identical(name, &pcm, &want, channels);
    }
}

/// MP4 files with AAC and Opus: ffmpeg honours the edit list's start; ffmpeg 6.1
/// decodes the last frame whole, so its output is ours plus the hidden
/// tail (less than one frame), while ffmpeg 9 also honours the
/// edit list's end. The rest agrees bit for bit.
#[test]
fn mp4_decodes_like_ffmpeg_up_to_its_untrimmed_end() {
    for name in MP4 {
        let (pcm, report, total, channels) = ours(name);
        assert!(report.warnings.is_empty(), "{name}: {:?}", report.warnings);
        assert_eq!(pcm.len() as u64, total * u64::from(channels), "{name}");
        let Some(want) = ffmpeg(name) else { return };
        let ch = usize::from(channels);
        assert!(want.len() >= pcm.len(), "{name}: ffmpeg is shorter");
        // An Opus packet may last up to 120 ms.
        let frame = if name.contains("opus") { 5760 } else { 1024 };
        assert!(want.len() - pcm.len() < frame * ch, "{name}: tail too long");
        assert_bit_identical(name, &pcm, &want[..pcm.len()], channels);
    }
}

/// The MP4 fixtures without perceptual noise substitution.
const MP4: [&str; 7] = [
    "mp4_aac_stereo_44k.m4a",
    "mp4_aac_mono_48k.m4a",
    "mp4_aac_noedit_22k.m4a",
    "mp4_aac_with_video.mp4",
    "mp4_aac_fragmented_44k.m4a",
    "mp4_opus_stereo.mp4",
    "video_h264_opus.mp4",
];

/// Files where our timeline deliberately differs from ffmpeg's (random
/// granule positions, chained streams, damage) still decode to exactly the
/// length of the packet index, so waveform and cut positions agree.
#[test]
fn every_fixture_decodes_to_the_index_length() {
    for name in [
        "ogg_vorbis_chained.ogg",
        "ogg_opus_chained.opus",
        "ogg_vorbis_badgranule.ogg",
        "ogg_opus_badgranule.opus",
        "ogg_vorbis_damaged.ogg",
        "mp3_notag_crc.mp3",
        "mp3_junk.mp3",
        "mp4_aac_pns_44k.m4a",
        "adts_aac_pns_44k.aac",
    ] {
        let (pcm, report, total, channels) = ours(name);
        assert_eq!(report.frames, total, "{name}");
        assert_eq!(pcm.len() as u64, total * u64::from(channels), "{name}");
    }
}

/// The value of sample *n* in [`ramp_wav`].
fn ramp(n: i32) -> i16 {
    (n * 7 - 3000) as i16
}

/// 16-bit mono WAV whose sample *n* has the value [`ramp`]`(n)`.
fn ramp_wav(frames: i32) -> Vec<u8> {
    let data: Vec<u8> = (0..frames).flat_map(|n| ramp(n).to_le_bytes()).collect();
    let mut f = b"RIFF".to_vec();
    f.extend((36 + data.len() as u32).to_le_bytes());
    f.extend(b"WAVEfmt ");
    f.extend(16u32.to_le_bytes());
    f.extend(1u16.to_le_bytes()); // PCM
    f.extend(1u16.to_le_bytes()); // mono
    f.extend(8000u32.to_le_bytes());
    f.extend(16000u32.to_le_bytes());
    f.extend(2u16.to_le_bytes());
    f.extend(16u16.to_le_bytes());
    f.extend(b"data");
    f.extend((data.len() as u32).to_le_bytes());
    f.extend(data);
    f
}

#[test]
fn wav_samples_are_scaled_exactly() {
    let wav = ramp_wav(5000);
    let mut r = Cursor::new(&wav);
    let c = lac_container::open(&mut r).expect("opens");
    let (pcm, report) = decode_to_vec(c.as_ref(), &mut r).expect("decodes");
    assert!(report.warnings.is_empty());
    let want: Vec<f32> = (0..5000).map(|n| f32::from(ramp(n)) / 32768.0).collect();
    assert_eq!(pcm, want);
}

#[test]
fn sink_can_stop_decoding() {
    let mut r = BufReader::new(File::open(fixture_path("mp3_cbr128_44k.mp3")).expect("fixture"));
    let c = lac_container::open(&mut r).expect("opens");
    let mut chunks = 0;
    let res = decode_all(c.as_ref(), &mut r, |_| {
        chunks += 1;
        if chunks == 3 {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });
    assert!(matches!(res, Err(lac_decode::DecodeError::Stopped)));
    assert_eq!(chunks, 3);
}

/// Every fixture with its chained and damaged variants.
const ALL: [&str; 47] = [
    "adts_aac_stereo_44k.aac",
    "adts_aac_mono_22k.aac",
    "ac3_stereo_44k.ac3",
    "ac3_51_48k.ac3",
    "ac3_mono_32k.ac3",
    "mp4_alac_stereo16_44k.m4a",
    "mp4_alac_mono24_48k.m4a",
    "mp4_alac_stereo24_44k.m4a",
    "mp4_aac_stereo_44k.m4a",
    "mp4_aac_mono_48k.m4a",
    "mp4_aac_noedit_22k.m4a",
    "mp4_aac_with_video.mp4",
    "mp4_aac_fragmented_44k.m4a",
    "mp4_alac_fragmented_48k.m4a",
    "aiff_s16_44k.aiff",
    "aiff_s24_48k_mono.aif",
    "aifc_sowt_22k.aifc",
    "aifc_fl32_22k.aifc",
    "flac_stereo16_44k.flac",
    "flac_mono8_8k_fixed.flac",
    "flac_quad24_48k.flac",
    "flac_wasted_22k.flac",
    "flac_leftside_ffmpeg.flac",
    "flac_rightside_ffmpeg.flac",
    "ogg_vorbis_stereo_44k.ogg",
    "ogg_vorbis_mono_22k.ogg",
    "ogg_opus_stereo.opus",
    "ogg_speex_16k.spx",
    "mp3_cbr128_44k.mp3",
    "mp3_vbr_44k.mp3",
    "mp3_mpeg2_mono_22k.mp3",
    "mp3_mpeg25_8k.mp3",
    "mp3_ffmpeg_id3v2.mp3",
    "ogg_vorbis_chained.ogg",
    "ogg_opus_chained.opus",
    "ogg_vorbis_badgranule.ogg",
    "ogg_opus_badgranule.opus",
    "ogg_vorbis_damaged.ogg",
    "mp3_notag_crc.mp3",
    "mp3_junk.mp3",
    "mkv_opus_stereo.webm",
    "mkv_opus_mono_10ms.mka",
    "mkv_opus_live.webm",
    "mkv_opus_damaged.webm",
    "mkv_opus_with_video.mkv",
    "mp4_opus_stereo.mp4",
    "video_h264_opus.mp4",
];

/// Ranges at the start, the end, around section boundaries and spread over
/// the file, from one frame to many packets.
fn probe_ranges(total: u64, boundaries: &[u64]) -> Vec<std::ops::Range<u64>> {
    let mut v = vec![
        0..1.min(total),
        0..total.min(5000),
        total.saturating_sub(3000)..total,
        total - 1..total,
    ];
    for &b in boundaries {
        v.push(b.saturating_sub(700)..(b + 700).min(total));
    }
    for k in 1..8u64 {
        let a = total * k / 8;
        for len in [1, 333, 4000] {
            v.push(a..(a + len).min(total));
        }
    }
    v
}

/// Largest absolute difference random access may have from a whole-file
/// decode: none for codecs whose decoders have finite memory, a small one
/// for the IIR state of Opus and Speex.
fn range_tolerance(name: &str) -> f32 {
    if name.contains("opus") || name.contains("speex") || name.ends_with(".spx") {
        TOLERANCE_IIR
    } else {
        0.0
    }
}

/// Largest difference allowed for Opus and Speex (one 16-bit LSB, as for
/// Speex joins in issue #17), see [`range_tolerance`]. The fixtures decode
/// bit for bit with the pre-roll of `decode_range`, but that is not
/// guaranteed for IIR decoders.
const TOLERANCE_IIR: f32 = 1.0 / 32768.0;

#[test]
fn decode_range_matches_decode_all() {
    for name in ALL {
        let mut r = BufReader::new(File::open(fixture_path(name)).expect("fixture"));
        let c = lac_container::open(&mut r).expect("opens");
        let ch = usize::from(c.index().info().channels);
        let total = c.index().total_samples();
        let (all, _) = decode_to_vec(c.as_ref(), &mut r).expect("decodes");
        // Timeline positions where one decode section ends and the next
        // begins (chained Ogg).
        let mut boundaries = Vec::new();
        let mut pos = 0;
        for s in c.decode_sections(&mut r).expect("sections") {
            pos += s.keep.end - s.keep.start;
            boundaries.push(pos);
        }
        let tol = range_tolerance(name);
        let mut worst = 0f32;
        for range in probe_ranges(total, &boundaries) {
            let got = lac_decode::decode_range(c.as_ref(), &mut r, range.clone()).expect("decodes");
            let want = &all[range.start as usize * ch..range.end as usize * ch];
            assert_eq!(got.len(), want.len(), "{name} {range:?}: length");
            for (i, (g, w)) in got.iter().zip(want).enumerate() {
                let d = (g - w).abs();
                worst = worst.max(d);
                assert!(
                    d <= tol,
                    "{name} {range:?}: frame {} channel {} is {g}, whole-file decode says {w}",
                    range.start + (i / ch) as u64,
                    i % ch
                );
            }
        }
        eprintln!("{name}: worst difference {worst:e}");
    }
}

#[test]
fn decode_range_clamps_to_the_timeline() {
    let mut r =
        BufReader::new(File::open(fixture_path("flac_stereo16_44k.flac")).expect("fixture"));
    let c = lac_container::open(&mut r).expect("opens");
    let total = c.index().total_samples();
    let got =
        lac_decode::decode_range(c.as_ref(), &mut r, total - 10..total + 100).expect("decodes");
    assert_eq!(got.len(), 10 * 2);
    assert!(
        lac_decode::decode_range(c.as_ref(), &mut r, total + 5..total + 9)
            .expect("decodes")
            .is_empty()
    );
}

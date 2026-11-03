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
    let out = match Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(fixture_path(name))
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
const EXACT: [&str; 15] = [
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
        let Some(mut want) = ffmpeg(name) else { return };
        if name.contains("vorbis") {
            // Our Vorbis timeline ignores the end trim of the last granule
            // position (granules are not trusted, see docs/architecture.md),
            // so it may run up to one block longer than ffmpeg's; that tail
            // is decoder output too, just not trimmed.
            let extra = pcm.len() - want.len().min(pcm.len());
            assert!(
                extra / usize::from(channels) < 2048,
                "{name}: {extra} extra samples"
            );
            want.extend(&pcm[want.len()..]);
        }
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
}

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
const ALL: [&str; 22] = [
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

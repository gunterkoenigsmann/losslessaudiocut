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

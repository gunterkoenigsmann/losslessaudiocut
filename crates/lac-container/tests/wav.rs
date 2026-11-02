//! Tests of the WAV demuxer and muxer.
//!
//! Test files are built in memory by [`build_wav`] from deterministic
//! pseudo-random samples, so no fixtures are needed for this format.

mod common;

use std::io::Cursor;

use lac_container::ContainerError;
use lac_container::wav::{BLOCK_FRAMES, SampleEncoding, WavFile};
use lac_core::{EditList, SourceId, Tags, plan_cut};
use proptest::prelude::*;

use common::noise;

/// Parameters of a generated WAV file.
#[derive(Clone, Copy)]
struct Spec {
    tag: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    extensible: bool,
}

const PCM16_STEREO: Spec = Spec {
    tag: 1,
    channels: 2,
    rate: 44_100,
    bits: 16,
    extensible: false,
};

fn fmt_payload(s: Spec) -> Vec<u8> {
    let align = s.channels * s.bits.div_ceil(8);
    let mut f = Vec::new();
    f.extend((if s.extensible { 0xFFFEu16 } else { s.tag }).to_le_bytes());
    f.extend(s.channels.to_le_bytes());
    f.extend(s.rate.to_le_bytes());
    f.extend((s.rate * u32::from(align)).to_le_bytes());
    f.extend(align.to_le_bytes());
    f.extend(s.bits.to_le_bytes());
    if s.extensible {
        f.extend(22u16.to_le_bytes()); // cbSize
        f.extend(s.bits.to_le_bytes()); // valid bits
        f.extend(0u32.to_le_bytes()); // channel mask
        f.extend(s.tag.to_le_bytes());
        f.extend([
            0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
        ]);
    }
    f
}

fn chunk(id: &[u8; 4], payload: &[u8], declared: Option<u32>) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend(declared.unwrap_or(payload.len() as u32).to_le_bytes());
    c.extend(payload);
    if payload.len() % 2 == 1 {
        c.push(0);
    }
    c
}

/// Builds a WAV file. `before_data` chunks are inserted between `fmt ` and
/// `data`; `declared_data` overrides the data chunk's size field.
fn build_wav(s: Spec, data: &[u8], before_data: &[Vec<u8>], declared_data: Option<u32>) -> Vec<u8> {
    let mut body = b"WAVE".to_vec();
    body.extend(chunk(b"fmt ", &fmt_payload(s), None));
    for c in before_data {
        body.extend(c);
    }
    body.extend(chunk(b"data", data, declared_data));
    let mut f = b"RIFF".to_vec();
    f.extend((body.len() as u32).to_le_bytes());
    f.extend(body);
    f
}

fn open(bytes: &[u8]) -> Result<WavFile, ContainerError> {
    WavFile::open(&mut Cursor::new(bytes))
}

// ---------------------------------------------------------------- parsing

#[test]
fn parses_basic_pcm() {
    let data = noise(4 * 10_000, 1);
    let w = open(&build_wav(PCM16_STEREO, &data, &[], None)).expect("valid");
    assert_eq!(w.info().sample_rate, 44_100);
    assert_eq!(w.info().channels, 2);
    assert_eq!(w.encoding(), SampleEncoding::Pcm);
    assert_eq!(w.block_align(), 4);
    assert_eq!(w.index().total_samples(), 10_000);
    assert_eq!(
        w.index().packets().len(),
        10_000usize.div_ceil(BLOCK_FRAMES as usize)
    );
    assert!(w.warnings().is_empty());
}

#[test]
fn parses_extensible_four_channel_24_bit() {
    let s = Spec {
        tag: 1,
        channels: 4,
        rate: 48_000,
        bits: 24,
        extensible: true,
    };
    let w = open(&build_wav(s, &noise(12 * 100, 2), &[], None)).expect("valid");
    assert_eq!(w.info().channels, 4);
    assert_eq!(w.block_align(), 12);
    assert_eq!(w.index().total_samples(), 100);
}

#[test]
fn skips_unknown_chunks_with_odd_size() {
    let junk = chunk(b"junk", &[1, 2, 3], None);
    let w = open(&build_wav(PCM16_STEREO, &noise(400, 3), &[junk], None)).expect("valid");
    assert_eq!(w.index().total_samples(), 100);
}

#[test]
fn placeholder_data_size_uses_rest_of_file() {
    for declared in [0, u32::MAX] {
        let w = open(&build_wav(
            PCM16_STEREO,
            &noise(400, 4),
            &[],
            Some(declared),
        ))
        .expect("valid");
        assert_eq!(w.index().total_samples(), 100);
        assert_eq!(w.warnings().len(), 1);
    }
}

#[test]
fn truncated_data_is_tolerated_with_warning() {
    let w = open(&build_wav(PCM16_STEREO, &noise(400, 5), &[], Some(800))).expect("valid");
    assert_eq!(w.index().total_samples(), 100);
    assert!(!w.warnings().is_empty());
}

#[test]
fn trailing_partial_frame_is_ignored() {
    let w = open(&build_wav(PCM16_STEREO, &noise(402, 6), &[], None)).expect("valid");
    assert_eq!(w.index().total_samples(), 100);
    assert_eq!(w.data_len(), 400);
}

#[test]
fn rejects_non_wave() {
    assert!(matches!(
        open(b"RIFX\0\0\0\0WAVE"),
        Err(ContainerError::Malformed { .. })
    ));
    assert!(matches!(
        open(b"RIFF"),
        Err(ContainerError::Malformed { .. })
    ));
}

#[test]
fn rejects_compressed_wav() {
    let adpcm = Spec {
        tag: 0x0011,
        ..PCM16_STEREO
    };
    assert!(matches!(
        open(&build_wav(adpcm, &noise(400, 7), &[], None)),
        Err(ContainerError::Unsupported(_))
    ));
}

#[test]
fn rejects_missing_fmt() {
    let mut f = b"RIFF\x0c\0\0\0WAVE".to_vec();
    f.extend(chunk(b"data", &[0; 4], None));
    assert!(matches!(open(&f), Err(ContainerError::Malformed { .. })));
}

#[test]
fn rejects_inconsistent_block_align() {
    let mut f = build_wav(PCM16_STEREO, &noise(400, 8), &[], None);
    // block_align lives at offset 12 + 8 + 12 = 32.
    f[32] = 1;
    assert!(matches!(open(&f), Err(ContainerError::Malformed { .. })));
}

// ---------------------------------------------------------------- cutting

/// Cuts `src` with the given delete ranges (in timeline coordinates,
/// applied in order) and returns the output file.
fn cut(src: &[u8], deletes: &[(u64, u64)]) -> (WavFile, Vec<u8>) {
    let w = open(src).expect("valid source");
    let mut edit = EditList::whole(SourceId(0), w.index().total_samples(), Tags::default());
    for &(a, b) in deletes {
        edit.tracks[0].delete(a..b).expect("valid delete");
    }
    let plans: Vec<_> = edit.tracks[0]
        .segments
        .iter()
        .map(|s| plan_cut(w.index(), w.capabilities(), s.range.clone()).expect("plan"))
        .collect();
    let mut out = Vec::new();
    w.write_cut(&mut Cursor::new(src), &plans, &mut out)
        .expect("write");
    (w, out)
}

#[test]
fn delete_middle_and_join() {
    let data = noise(4 * 10_000, 9);
    let src = build_wav(PCM16_STEREO, &data, &[], None);
    let (_, out) = cut(&src, &[(1_000, 6_000)]);
    let o = open(&out).expect("output is valid WAV");
    assert_eq!(o.index().total_samples(), 5_000);
    let got = o
        .read_frames(&mut Cursor::new(&out), 0..5_000)
        .expect("read");
    let mut want = data[..4_000].to_vec();
    want.extend(&data[24_000..]);
    assert_eq!(got, want);
}

#[test]
fn output_keeps_fmt_chunk_verbatim() {
    let s = Spec {
        tag: 3,
        channels: 3,
        rate: 96_000,
        bits: 32,
        extensible: true,
    };
    let src = build_wav(s, &noise(12 * 500, 10), &[], None);
    let (_, out) = cut(&src, &[(10, 20)]);
    let fmt = fmt_payload(s);
    assert_eq!(&out[20..20 + fmt.len()], &fmt[..]);
    assert_eq!(open(&out).expect("valid").encoding(), SampleEncoding::Float);
}

#[test]
fn odd_length_output_is_padded() {
    // 8-bit mono: one byte per frame, so 101 frames give an odd data chunk.
    let s = Spec {
        tag: 1,
        channels: 1,
        rate: 8_000,
        bits: 8,
        extensible: false,
    };
    let src = build_wav(s, &noise(200, 11), &[], None);
    let (_, out) = cut(&src, &[(0, 99)]);
    assert_eq!(out.len() % 2, 0);
    let riff_size = u32::from_le_bytes([out[4], out[5], out[6], out[7]]) as usize;
    assert_eq!(riff_size + 8, out.len());
    assert_eq!(open(&out).expect("valid").index().total_samples(), 101);
}

#[test]
fn delete_everything_gives_valid_empty_file() {
    let src = build_wav(PCM16_STEREO, &noise(400, 12), &[], None);
    let (_, out) = cut(&src, &[(0, 100)]);
    let o = open(&out).expect("valid");
    assert_eq!(o.index().total_samples(), 0);
}

proptest! {
    /// Golden rule: the output's samples equal the source's samples with
    /// the deleted ranges removed, bit for bit.
    #[test]
    fn cut_matches_reference(
        frames in 0u64..20_000,
        channels in 1u16..5,
        bytes_per_sample in 1u16..5,
        deletes in proptest::collection::vec((0u64..20_000, 0u64..8_000), 0..5),
        seed in any::<u64>(),
    ) {
        let s = Spec {
            tag: if bytes_per_sample == 4 { 3 } else { 1 },
            channels,
            rate: 48_000,
            bits: bytes_per_sample * 8,
            extensible: channels > 2,
        };
        let ba = u64::from(channels * bytes_per_sample);
        let data = noise((frames * ba) as usize, seed);
        let src = build_wav(s, &data, &[], None);

        // Reference model: list of frames, deletes applied in order.
        let mut model: Vec<&[u8]> = data.chunks(ba as usize).collect();
        let mut dels = Vec::new();
        for (a, n) in deletes {
            let len = model.len() as u64;
            let a = a.min(len);
            let b = (a + n).min(len);
            model.drain(a as usize..b as usize);
            dels.push((a, b));
        }
        let (_, out) = cut(&src, &dels);
        let o = open(&out).expect("output is valid WAV");
        prop_assert_eq!(o.index().total_samples(), model.len() as u64);
        let got = o.read_frames(&mut Cursor::new(&out), 0..model.len() as u64).expect("read");
        prop_assert_eq!(got, model.concat());
    }
}

// ------------------------------------------------- independent cross-check

/// Decodes `wav` with ffmpeg to raw bytes in its own sample format.
fn ffmpeg_raw(wav: &[u8], raw_format: &str) -> Option<Vec<u8>> {
    let path = common::temp_file(&format!("wav-{raw_format}.wav"), wav);
    let out = common::ffmpeg_decode(&path, raw_format);
    let _ = std::fs::remove_file(&path);
    out
}

#[test]
fn ffmpeg_agrees_with_our_output() {
    for (spec, raw) in [
        (PCM16_STEREO, "s16le"),
        (
            Spec {
                tag: 1,
                channels: 4,
                rate: 48_000,
                bits: 24,
                extensible: true,
            },
            "s24le",
        ),
        (
            Spec {
                tag: 3,
                channels: 1,
                rate: 32_000,
                bits: 32,
                extensible: false,
            },
            "f32le",
        ),
    ] {
        let ba = u64::from(spec.channels * spec.bits / 8);
        let data = noise((30_000 * ba) as usize, 13);
        let src = build_wav(spec, &data, &[], None);
        let (_, out) = cut(&src, &[(100, 7_000), (20_000, 22_000)]);
        let Some(decoded) = ffmpeg_raw(&out, raw) else {
            return;
        };
        let mut want = data[..(100 * ba) as usize].to_vec();
        want.extend(&data[(7_000 * ba) as usize..(26_900 * ba) as usize]);
        want.extend(&data[(28_900 * ba) as usize..]);
        assert_eq!(decoded.len(), want.len(), "{raw}: length differs");
        assert!(decoded == want, "{raw}: ffmpeg decodes different samples");
    }
}

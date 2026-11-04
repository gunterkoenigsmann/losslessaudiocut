//! Tests of the FLAC demuxer, boundary re-encoder and muxer.
//!
//! Fixtures come from `tests/fixtures/generate.sh` (libFLAC and ffmpeg
//! encoders, several bit depths, channel counts and stereo modes).
//!
//! Oracles, from strongest to weakest independence:
//! 1. `flac --test` checks every CRC and the STREAMINFO MD5 of our output.
//! 2. ffmpeg decodes our output and the source; samples must match.
//! 3. Our own decoder (itself checked against ffmpeg) compares output and
//!    source in property tests, which run without external tools.

mod common;

use std::io::Cursor;

use common::{ffmpeg_decode, fixture, run_tool, temp_file};
use lac_container::flac::FlacFile;
use lac_container::flac::decode::Channels;
use lac_core::{CutPlan, EditList, SourceId, Tags, plan_cut};
use proptest::prelude::*;

const FIXTURES: [&str; 6] = [
    "flac_stereo16_44k.flac",
    "flac_quad24_48k.flac",
    "flac_mono8_8k_fixed.flac",
    "flac_wasted_22k.flac",
    "flac_leftside_ffmpeg.flac",
    "flac_rightside_ffmpeg.flac",
];

fn open(bytes: &[u8]) -> FlacFile {
    FlacFile::open(&mut Cursor::new(bytes)).expect("valid FLAC")
}

fn decode_all(f: &FlacFile, bytes: &[u8]) -> Channels {
    f.decode_range(&mut Cursor::new(bytes), 0..f.index().total_samples())
        .expect("decodes")
}

/// Writes the given plans of `src` to a new FLAC file.
fn write(src: &[u8], f: &FlacFile, plans: &[CutPlan]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    f.write_cut(&mut Cursor::new(src), plans, None, &mut out)
        .expect("write");
    out.into_inner()
}

/// Keeps the given source ranges, in order.
fn keep(src: &[u8], ranges: &[std::ops::Range<u64>]) -> Vec<u8> {
    let f = open(src);
    let plans: Vec<_> = ranges
        .iter()
        .map(|r| plan_cut(f.index(), f.capabilities(), r.clone()).expect("plan"))
        .collect();
    write(src, &f, &plans)
}

/// Deletes timeline ranges one after the other, like the editor does.
fn delete(src: &[u8], deletes: &[(u64, u64)]) -> (Vec<u8>, Vec<std::ops::Range<u64>>) {
    let f = open(src);
    let mut edit = EditList::whole(SourceId(0), f.index().total_samples(), Tags::default());
    for &(a, b) in deletes {
        edit.tracks[0].delete(a..b).expect("valid delete");
    }
    let ranges: Vec<_> = edit.tracks[0]
        .segments
        .iter()
        .map(|s| s.range.clone())
        .collect();
    (keep(src, &ranges), ranges)
}

/// The source's samples restricted to `ranges`, joined.
fn expected(src: &[u8], ranges: &[std::ops::Range<u64>]) -> Channels {
    let f = open(src);
    let all = decode_all(&f, src);
    all.iter()
        .map(|c| {
            ranges
                .iter()
                .flat_map(|r| c[r.start as usize..r.end as usize].iter().copied())
                .collect()
        })
        .collect()
}

/// Runs `flac --test` on `data`; panics if the reference decoder rejects it.
fn flac_test(name: &str, data: &[u8]) {
    let path = temp_file(name, data);
    if let Some(o) = run_tool(
        "flac",
        &["--test".as_ref(), "-s".as_ref(), path.as_os_str()],
    ) {
        assert!(
            o.status.success(),
            "flac --test rejects {name}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    let _ = std::fs::remove_file(path);
}

// ------------------------------------------------------------ demuxing

#[test]
fn fixtures_open_cleanly() {
    for name in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        assert!(f.warnings().is_empty(), "{name}: {:?}", f.warnings());
        assert_eq!(
            f.index().total_samples(),
            f.stream_info().total_samples,
            "{name}"
        );
        assert!(f.index().packets().len() > 1, "{name}");
    }
}

#[test]
fn decoder_matches_ffmpeg() {
    for name in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let Some(raw) = ffmpeg_decode(&common::fixture_path(name), "s32le") else {
            return;
        };
        // ffmpeg left-aligns samples in 32 bits.
        let shift = 32 - f.stream_info().bits_per_sample;
        let chans = decode_all(&f, &src);
        let ours: Vec<u8> = (0..chans[0].len())
            .flat_map(|i| chans.iter().map(move |c| c[i] << shift))
            .flat_map(i32::to_le_bytes)
            .collect();
        assert_eq!(ours.len(), raw.len(), "{name}: length");
        assert!(ours == raw, "{name}: decoded samples differ from ffmpeg");
    }
}

#[test]
fn rejects_non_flac() {
    assert!(FlacFile::open(&mut Cursor::new(b"OggS\0\0\0\0\0\0\0\0".to_vec())).is_err());
    assert!(FlacFile::open(&mut Cursor::new(b"fLa".to_vec())).is_err());
}

#[test]
fn id3v2_prefix_is_skipped() {
    let src = fixture("flac_mono8_8k_fixed.flac");
    // ID3v2.4 header with a 20-byte (syncsafe) body of zeros.
    let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x14".to_vec();
    tagged.extend([0u8; 20]);
    tagged.extend(&src);
    let f = FlacFile::open(&mut Cursor::new(tagged.clone())).expect("opens");
    assert_eq!(f.warnings().len(), 1);
    assert_eq!(decode_all(&f, &tagged), decode_all(&open(&src), &src));
}

#[test]
fn damaged_frame_is_skipped_with_warning() {
    let src = fixture("flac_stereo16_44k.flac");
    let clean = open(&src);
    let victim = clean.index().packets()[3];
    let mut bad = src.clone();
    bad[(victim.byte_offset + u64::from(victim.byte_len) / 2) as usize] ^= 0x55;
    let f = open(&bad);
    assert!(!f.warnings().is_empty());
    assert_eq!(
        f.index().total_samples(),
        clean.index().total_samples() - u64::from(victim.n_samples)
    );
    // The remaining audio is the clean audio without the damaged frame.
    let all = decode_all(&clean, &src);
    let r = victim.sample_range();
    let want: Channels = all
        .iter()
        .map(|c| [&c[..r.start as usize], &c[r.end as usize..]].concat())
        .collect();
    assert_eq!(decode_all(&f, &bad), want);
}

#[test]
fn truncated_file_drops_partial_last_frame() {
    let src = fixture("flac_wasted_22k.flac");
    let clean = open(&src);
    let cut = &src[..src.len() - 100];
    let f = open(cut);
    assert!(!f.warnings().is_empty());
    let last = clean.index().packets().last().expect("frames");
    assert_eq!(
        f.index().total_samples(),
        clean.index().total_samples() - u64::from(last.n_samples)
    );
}

// ------------------------------------------------------------- cutting

#[test]
fn cut_output_passes_reference_tools() {
    for name in FIXTURES {
        let src = fixture(name);
        let len = open(&src).index().total_samples();
        // One cut in the middle of frames, one exactly on the stream end.
        let (out, ranges) = delete(&src, &[(len / 7, len / 3), (len / 2, len - len / 2)]);
        let o = open(&out);
        assert!(o.warnings().is_empty(), "{name}: {:?}", o.warnings());
        assert_eq!(decode_all(&o, &out), expected(&src, &ranges), "{name}");
        flac_test(&format!("cut-{name}"), &out);

        // Independent decoder on both sides.
        let out_path = temp_file(&format!("ff-{name}"), &out);
        let (Some(a), Some(b)) = (
            ffmpeg_decode(&out_path, "s32le"),
            ffmpeg_decode(&common::fixture_path(name), "s32le"),
        ) else {
            return;
        };
        let frame = 4 * usize::from(o.stream_info().channels);
        let want: Vec<u8> = ranges
            .iter()
            .flat_map(|r| {
                b[r.start as usize * frame..r.end as usize * frame]
                    .iter()
                    .copied()
            })
            .collect();
        assert!(a == want, "{name}: ffmpeg decodes different samples");
    }
}

#[test]
fn tiny_segments_are_valid() {
    let src = fixture("flac_stereo16_44k.flac");
    let ranges = [
        10..13,
        5000..5001,
        4600..4615,
        20_000..20_100,
        30_000..30_007,
    ];
    let out = keep(&src, &ranges);
    assert_eq!(decode_all(&open(&out), &out), expected(&src, &ranges));
    flac_test("tiny.flac", &out);
}

#[test]
fn cut_of_a_cut() {
    // Our output is a variable-block-size stream; it must be cuttable too.
    let src = fixture("flac_quad24_48k.flac");
    let (once, _) = delete(&src, &[(1_000, 2_500)]);
    let (twice, ranges) = delete(&once, &[(3_000, 4_000), (0, 17)]);
    assert_eq!(decode_all(&open(&twice), &twice), expected(&once, &ranges));
    flac_test("twice.flac", &twice);
}

#[test]
fn delete_everything_gives_valid_empty_file() {
    let src = fixture("flac_mono8_8k_fixed.flac");
    let len = open(&src).index().total_samples();
    let (out, _) = delete(&src, &[(0, len)]);
    assert_eq!(open(&out).index().total_samples(), 0);
    flac_test("empty.flac", &out);
}

#[test]
fn tags_are_kept_and_seektable_dropped() {
    let src = fixture("flac_wasted_22k.flac");
    let (out, _) = delete(&src, &[(100, 5_000)]);
    let types = block_types(&out);
    assert_eq!(types[0], 0, "STREAMINFO first");
    assert!(types.contains(&4), "VORBIS_COMMENT kept");
    assert!(!types.contains(&3), "SEEKTABLE dropped");
    assert!(
        out.windows(13).any(|w| w == b"TITLE=Fixture"),
        "title tag survives"
    );
}

/// Types of all metadata blocks of a FLAC file.
fn block_types(f: &[u8]) -> Vec<u8> {
    let mut pos = 4;
    let mut types = Vec::new();
    loop {
        let h = &f[pos..pos + 4];
        types.push(h[0] & 0x7F);
        pos += 4 + u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        if h[0] & 0x80 != 0 {
            return types;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Golden rule: random delete sequences on every fixture give exactly
    /// the source samples outside the deleted ranges.
    #[test]
    fn random_deletes_are_bit_exact(
        which in 0usize..FIXTURES.len(),
        deletes in proptest::collection::vec((0.0f64..1.0, 0.0f64..0.3), 1..5),
    ) {
        let src = fixture(FIXTURES[which]);
        let len = open(&src).index().total_samples();
        // Map relative positions onto the shrinking timeline.
        let mut cur = len;
        let mut abs = Vec::new();
        for (a, n) in deletes {
            let start = (a * cur as f64) as u64;
            let end = (start + (n * len as f64) as u64).min(cur);
            abs.push((start, end));
            cur -= end - start;
        }
        let (out, ranges) = delete(&src, &abs);
        let o = open(&out);
        prop_assert!(o.warnings().is_empty());
        prop_assert_eq!(decode_all(&o, &out), expected(&src, &ranges));
    }
}

//! Tests of the MP3 demuxer and muxer.
//!
//! Fixtures come from `tests/fixtures/generate.sh` (LAME and ffmpeg's
//! libmp3lame: CBR, VBR, MPEG-1/2/2.5, mono and stereo, with and without
//! LAME tag, CRC-protected frames, ID3v1/ID3v2 tags, junk in the middle).
//!
//! Oracles:
//! 1. Nothing is re-encoded: for every kept frame the side information
//!    (apart from the reservoir pointer `main_data_begin`) and the main data
//!    bytes are the same in the output, read back by our demuxer.
//! 2. ffmpeg decodes output and source (gapless): the output must equal the
//!    source's samples in the planned ranges, bit for bit, except within
//!    [`JOIN_WINDOW`] after a join point (see "Join points inside a file" in
//!    `docs/architecture.md`). This also checks the repacked bit reservoir
//!    and the new LAME tag (delay and padding), since any error there shifts
//!    or corrupts the output.
//! 3. mpg123, a second, independent gapless decoder, must agree on the
//!    length and the timing. Its float output is not bit-reproducible at a
//!    different position in the stream: its synthesis filter's ring buffer
//!    phase depends on the absolute granule number, which shifts the
//!    rounding of mono streams by one unit in the last place. So it is
//!    compared within [`MPG123_TOLERANCE`]; ffmpeg remains the bit-exact
//!    oracle.

// Lists of kept ranges often hold a single range here.
#![allow(clippy::single_range_in_vec_init)]

mod common;

use std::io::Cursor;
use std::ops::Range;
use std::path::Path;

use common::{ffmpeg_decode, fixture, run_tool, temp_file};
use lac_container::mp3::Mp3File;
use lac_core::{CutPlan, EditList, SourceId, Tags, plan_cuts};
use proptest::prelude::*;

/// (name, sample rate, channels, length in samples).
const FIXTURES: [(&str, u32, u16, u64); 6] = [
    ("mp3_cbr128_44k.mp3", 44_100, 2, 66_150),
    ("mp3_vbr_44k.mp3", 44_100, 2, 66_150),
    ("mp3_mpeg2_mono_22k.mp3", 22_050, 1, 22_050),
    // No LAME tag (its 144-byte frames are too small): length is whole frames.
    ("mp3_mpeg25_8k.mp3", 8_000, 1, 9_216),
    ("mp3_notag_crc.mp3", 44_100, 2, 67_968),
    ("mp3_ffmpeg_id3v2.mp3", 44_100, 2, 66_150),
];

/// Samples after a join point in which the output may differ from the
/// source: the first frame after the join overlaps with a foreign
/// predecessor granule (one MPEG-1 frame, or two MPEG-2 frames).
const JOIN_WINDOW: usize = 1152;

/// Largest sample difference accepted from mpg123 (see oracle 3): a few
/// units in the last place of an f32 near full scale (2^-23 each), far
/// below anything audible (-120 dBFS).
const MPG123_TOLERANCE: f32 = 1e-6;

fn open(bytes: &[u8]) -> Mp3File {
    Mp3File::open(&mut Cursor::new(bytes)).expect("valid MP3")
}

fn write(src: &[u8], f: &Mp3File, plans: &[CutPlan]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    f.write_cut(&mut Cursor::new(src), plans, &mut out)
        .expect("write");
    out.into_inner()
}

fn plans(f: &Mp3File, ranges: &[Range<u64>]) -> Vec<CutPlan> {
    plan_cuts(f.index(), f.capabilities(), ranges).expect("plan")
}

/// Plans of what remains after deleting timeline ranges one by one.
fn delete(f: &Mp3File, deletes: &[(u64, u64)]) -> Vec<CutPlan> {
    let mut edit = EditList::whole(SourceId(0), f.index().total_samples(), Tags::default());
    for &(a, b) in deletes {
        edit.tracks[0].delete(a..b).expect("valid delete");
    }
    let ranges: Vec<_> = edit.tracks[0]
        .segments
        .iter()
        .map(|s| s.range.clone())
        .collect();
    plans(f, &ranges)
}

/// Oracle 1; returns the re-opened output.
fn assert_nothing_reencoded(src: &[u8], out: &[u8], plans: &[CutPlan]) -> Mp3File {
    let f = open(src);
    let o = open(out);
    assert!(o.warnings().is_empty(), "{:?}", o.warnings());
    assert_eq!(o.index().info(), f.index().info());
    let mut rs = Cursor::new(src);
    let mut ro = Cursor::new(out);
    let want: Vec<_> = plans
        .iter()
        .flat_map(|p| p.packets.clone())
        .map(|e| f.audio_data(&mut rs, e).expect("source frame"))
        .collect();
    let got: Vec<_> = (0..o.index().packets().len())
        .map(|e| o.audio_data(&mut ro, e).expect("output frame"))
        .collect();
    assert_eq!(got.len(), want.len(), "frame count");
    assert!(got == want, "frame contents differ");
    let len: u64 = plans.iter().map(|p| p.actual.end - p.actual.start).sum();
    assert_eq!(o.index().total_samples(), len, "timeline of the output");
    o
}

/// Decodes with mpg123 (gapless mode is its default) to f32 samples.
fn mpg123_decode(path: &Path) -> Option<Vec<u8>> {
    let o = run_tool(
        "mpg123",
        &[
            "-q".as_ref(),
            "-s".as_ref(),
            "-e".as_ref(),
            "f32".as_ref(),
            path.as_os_str(),
        ],
    )?;
    assert!(o.status.success(), "mpg123 failed");
    Some(o.stdout)
}

/// Oracles 2 and 3 for one decoder; `tolerance` 0 means bit for bit.
fn assert_decodes_like_source(
    name: &str,
    decoder: fn(&Path) -> Option<Vec<u8>>,
    tolerance: f32,
    src: &[u8],
    out: &[u8],
    plans: &[CutPlan],
) {
    let f = open(src);
    let frame = 4 * usize::from(f.index().info().channels);
    let sp = temp_file(&format!("src-{name}"), src);
    let op = temp_file(&format!("out-{name}"), out);
    let (Some(s), Some(o)) = (decoder(&sp), decoder(&op)) else {
        return;
    };
    let total: u64 = plans.iter().map(|p| p.actual.end - p.actual.start).sum();
    assert_eq!(o.len() / frame, total as usize, "{name}: decoded length");
    assert_eq!(
        s.len() / frame,
        f.index().total_samples() as usize,
        "{name}: source length"
    );
    let mut out_pos = 0;
    let mut prev_end = None;
    for p in plans {
        let r = p.actual.start as usize..p.actual.end as usize;
        // A range continuing the previous one is no join.
        let skip = match prev_end {
            Some(e) if e != p.actual.start => JOIN_WINDOW.min(r.len()),
            _ => 0,
        };
        let want = &s[(r.start + skip) * frame..r.end * frame];
        let got = &o[(out_pos + skip) * frame..(out_pos + r.len()) * frame];
        let sample = |b: &[u8], i: usize| {
            f32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
        };
        if let Some(i) = (0..want.len() / 4).find(|&i| {
            let (a, b) = (sample(want, i), sample(got, i));
            if tolerance == 0.0 {
                a.to_bits() != b.to_bits()
            } else {
                (a - b).abs() > tolerance
            }
        }) {
            let i = i / usize::from(f.index().info().channels);
            panic!(
                "{name}: range {r:?} differs at source sample {} ({i} samples after the allowed window)",
                r.start + skip + i
            );
        }
        out_pos += r.len();
        prev_end = Some(p.actual.end);
    }
    let _ = std::fs::remove_file(sp);
    let _ = std::fs::remove_file(op);
}

fn ffmpeg(path: &Path) -> Option<Vec<u8>> {
    ffmpeg_decode(path, "f32le")
}

/// All oracles.
fn check(name: &str, src: &[u8], out: &[u8], plans: &[CutPlan]) -> Mp3File {
    let o = assert_nothing_reencoded(src, out, plans);
    assert_decodes_like_source(name, ffmpeg, 0.0, src, out, plans);
    assert_decodes_like_source(name, mpg123_decode, MPG123_TOLERANCE, src, out, plans);
    o
}

// ------------------------------------------------------------ demuxing

#[test]
fn fixtures_open_cleanly() {
    for (name, rate, channels, len) in FIXTURES {
        let f = open(&fixture(name));
        assert!(f.warnings().is_empty(), "{name}: {:?}", f.warnings());
        assert_eq!(f.index().info().sample_rate, rate, "{name}");
        assert_eq!(f.index().info().channels, channels, "{name}");
        assert_eq!(f.index().total_samples(), len, "{name}");
    }
}

#[test]
fn junk_is_skipped_with_warning() {
    let clean = open(&fixture("mp3_cbr128_44k.mp3"));
    let src = fixture("mp3_junk.mp3");
    let f = open(&src);
    assert!(
        f.warnings()[0].contains("skipped 700 bytes"),
        "{:?}",
        f.warnings()
    );
    assert_eq!(f.index().total_samples(), clean.index().total_samples());
    // Cutting it gives a clean file.
    let len = f.index().total_samples();
    let p = plans(&f, &[0..len]);
    let out = write(&src, &f, &p);
    assert!(open(&out).warnings().is_empty());
}

#[test]
fn garbage_is_rejected_without_panic() {
    for data in [
        vec![0xFF, 0xFB],
        common::noise(20_000, 5),
        [
            b"ID3\x04\0\0\0\0\0\x10".as_slice(),
            &common::noise(5_000, 9),
        ]
        .concat(),
        // A Layer II header repeated.
        [0xFF, 0xFD, 0x90, 0x00].repeat(500),
    ] {
        assert!(Mp3File::open(&mut Cursor::new(&data)).is_err());
    }
}

#[test]
fn truncated_files_open() {
    let src = fixture("mp3_vbr_44k.mp3");
    for cut in [src.len() / 3, src.len() - 7] {
        let f = open(&src[..cut]);
        assert!(f.index().total_samples() > 0);
    }
}

// ------------------------------------------------------------ cutting

#[test]
fn whole_file_round_trips() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let p = plans(&f, &[0..f.index().total_samples()]);
        let out = write(&src, &f, &p);
        check(name, &src, &out, &p);
    }
}

/// Start and end in the middle of frames are sample-exact.
#[test]
fn keep_middle_part_is_sample_exact() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let r = len / 5 + 7..len * 4 / 5 - 3;
        let p = plans(&f, std::slice::from_ref(&r));
        assert_eq!(p[0].actual, r, "{name}");
        let out = write(&src, &f, &p);
        check(name, &src, &out, &p);
    }
}

#[test]
fn deletes_join_frames() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let p = delete(&f, &[(len / 6, len / 3), (len / 2, len * 2 / 3)]);
        let out = write(&src, &f, &p);
        check(name, &src, &out, &p);
    }
}

#[test]
fn tags_are_copied() {
    let src = fixture("mp3_ffmpeg_id3v2.mp3");
    let f = open(&src);
    let len = f.index().total_samples();
    let out = write(&src, &f, &plans(&f, &[len / 2..len]));
    // The ID3v2 tag in front, byte for byte.
    let tag_len = 10
        + src[6..10]
            .iter()
            .fold(0usize, |a, &b| (a << 7) | usize::from(b));
    assert_eq!(out[..tag_len], src[..tag_len]);

    let src = fixture("mp3_notag_crc.mp3");
    let f = open(&src);
    let out = write(&src, &f, &plans(&f, &[1000..20_000]));
    assert_eq!(out[out.len() - 128..], src[src.len() - 128..]);
    assert!(out[out.len() - 128..].starts_with(b"TAG"));
}

/// ffmpeg verifies the CRC-16 of protected frames, which change when
/// `main_data_begin` is rewritten.
#[test]
fn crc_protected_frames_stay_valid() {
    let name = "mp3_notag_crc.mp3";
    let src = fixture(name);
    let f = open(&src);
    let len = f.index().total_samples();
    let out = write(&src, &f, &delete(&f, &[(len / 4, len / 2)]));
    let path = temp_file(name, &out);
    if let Some(o) = run_tool(
        "ffmpeg",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-err_detect".as_ref(),
            "crccheck+explode".as_ref(),
            "-i".as_ref(),
            path.as_os_str(),
            "-f".as_ref(),
            "null".as_ref(),
            "-".as_ref(),
        ],
    ) {
        assert!(
            o.status.success() && o.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

#[test]
fn cut_of_cut_keeps_everything() {
    let name = "mp3_vbr_44k.mp3";
    let src = fixture(name);
    let f = open(&src);
    let len = f.index().total_samples();
    let once = write(&src, &f, &plans(&f, &[len / 3..len]));
    let o = open(&once);
    let p = plans(&o, &[0..o.index().total_samples()]);
    let twice = write(&once, &o, &p);
    check(name, &once, &twice, &p);
}

#[test]
fn inner_trims_are_rejected() {
    let src = fixture("mp3_cbr128_44k.mp3");
    let f = open(&src);
    // Planned without the file-ends restriction, the second range trims.
    let bad: Vec<_> = [0..10_000, 20_000..30_000]
        .into_iter()
        .map(|r| lac_core::plan_cut(f.index(), f.capabilities(), r).expect("plan"))
        .collect();
    let mut out = Cursor::new(Vec::new());
    assert!(f.write_cut(&mut Cursor::new(&src), &bad, &mut out).is_err());
}

#[test]
fn empty_cut_writes_nothing() {
    let src = fixture("mp3_cbr128_44k.mp3");
    let f = open(&src);
    assert!(write(&src, &f, &plans(&f, &[500..500])).is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Random delete sequences on every fixture: nothing re-encoded, the
    /// planned timeline, and (with ffmpeg and mpg123 installed) the source's
    /// samples outside the join windows.
    #[test]
    fn random_deletes(
        which in 0usize..FIXTURES.len(),
        deletes in proptest::collection::vec((0.0f64..1.0, 0.0f64..0.3), 1..4),
    ) {
        let name = FIXTURES[which].0;
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let mut cur = len;
        let mut abs = Vec::new();
        for (a, n) in deletes {
            let start = (a * cur as f64) as u64;
            let end = (start + (n * len as f64) as u64).min(cur);
            abs.push((start, end));
            cur -= end - start;
        }
        let p = delete(&f, &abs);
        let out = write(&src, &f, &p);
        if !p.iter().all(|p| p.packets.is_empty()) {
            check(name, &src, &out, &p);
        }
    }
}

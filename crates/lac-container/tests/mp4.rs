//! Tests of the MP4 demuxer and muxer with AAC and ALAC.
//!
//! Fixtures come from `tests/fixtures/generate.sh` (ffmpeg's AAC-LC
//! encoder: moov after and before mdat, without an edit list, and with a
//! video track; ffmpeg's ALAC encoder at 16 and 24 bit).
//!
//! Oracles:
//! 1. Samples are copied verbatim: re-opening the output with our demuxer
//!    yields exactly the kept source samples, byte for byte, and the
//!    planned length. This holds without external tools.
//! 2. ffmpeg (an independent demuxer and decoder) decodes the output like
//!    the source restricted to the plans' actual ranges, bit for bit,
//!    except within [`join_window`] after each join: there the first AAC
//!    frame overlaps (MDCT) with a frame that was not its predecessor.
//!    ALAC frames are independent, so ALAC has no such window.
//! 3. ffmpeg honours the edit list's start. Whether it honours its end
//!    depends on the version: ffmpeg 6.1 decodes the last frame whole, so
//!    its output is our planned length plus the hidden tail of the last
//!    sample (less than one frame, [`max_frame`]); ffmpeg 9 outputs exactly
//!    the planned length. Both are accepted. Our own decoding
//!    (`lac-decode`'s tests) honours both ends.
//!
//! Oracle 2 needs fixtures without perceptual noise substitution (PNS,
//! ISO/IEC 14496-3 4.6.13): a PNS band is filled with noise from a random
//! generator that libavcodec runs from the start of the stream, so a
//! decoder that starts anywhere else, as after any cut, draws different
//! noise of the same spectral envelope. The packets are still copied
//! verbatim; [`noise_substitution_differs_only_in_noise`] covers that case,
//! and `lac-decode/tests/pns.rs` checks in the transform domain that only
//! the noise bands change, keeping their energy (issue #72).

// Lists of kept ranges often hold a single range here.
#![allow(clippy::single_range_in_vec_init)]

mod common;

use std::io::Cursor;
use std::ops::Range;

use common::{ffmpeg_decode, fixture, temp_file};
use lac_container::mp4::{Mp4Codec, Mp4File};
use lac_core::{CutPlan, EditList, SourceId, Tags, plan_cuts};
use proptest::prelude::*;

/// Fixtures: (name, sample rate, channels, length in samples). With an
/// edit list the length is exactly the encoder's input; without one it
/// includes the 1024 priming samples.
const FIXTURES: [(&str, u32, u16, u64); 6] = [
    ("mp4_aac_stereo_44k.m4a", 44_100, 2, 66_150),
    ("mp4_aac_mono_48k.m4a", 48_000, 1, 48_000),
    ("mp4_aac_noedit_22k.m4a", 22_050, 2, 22_050 + 1024),
    ("mp4_aac_with_video.mp4", 48_000, 1, 48_000),
    ("mp4_alac_stereo16_44k.m4a", 44_100, 2, 33_075),
    ("mp4_alac_mono24_48k.m4a", 48_000, 1, 24_000),
];

/// The largest decoder output of one sample: an AAC-LC frame always
/// decodes to 1024 samples, ffmpeg's ALAC frames hold up to 4096.
fn max_frame(f: &Mp4File) -> u64 {
    match f.codec() {
        Mp4Codec::Aac { .. } => 1024,
        Mp4Codec::Alac { .. } => 4096,
    }
}

/// Samples after a join in which the output may differ from the source.
/// AAC: one frame, whose output is the overlap-add of its own first half
/// with the second half of the frame before the join (ISO/IEC 14496-3
/// 4.6.11); from the next frame on, the decoder state holds only the
/// current frame, so the output is bit-exact again. ALAC: none, every
/// frame decodes on its own.
fn join_window(f: &Mp4File) -> usize {
    match f.codec() {
        Mp4Codec::Aac { .. } => 1024,
        Mp4Codec::Alac { .. } => 0,
    }
}

fn open(bytes: &[u8]) -> Mp4File {
    Mp4File::open(&mut Cursor::new(bytes)).expect("valid MP4")
}

fn write(src: &[u8], f: &Mp4File, plans: &[CutPlan]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    f.write_cut(&mut Cursor::new(src), plans, None, &mut out)
        .expect("write");
    out.into_inner()
}

fn plans(f: &Mp4File, ranges: &[Range<u64>]) -> Vec<CutPlan> {
    plan_cuts(f.index(), f.capabilities(), ranges).expect("plan")
}

/// Deletes timeline ranges one after the other, like the editor does, and
/// returns the plans of the remaining source ranges.
fn delete(f: &Mp4File, deletes: &[(u64, u64)]) -> Vec<CutPlan> {
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

fn payloads(f: &Mp4File, bytes: &[u8], entries: impl Iterator<Item = usize>) -> Vec<Vec<u8>> {
    let mut r = Cursor::new(bytes);
    entries
        .map(|e| f.read_packet(&mut r, e).expect("packet"))
        .collect()
}

/// Checks oracle 1 and returns the re-opened output.
fn assert_samples_copied(src: &[u8], out: &[u8], plans: &[CutPlan]) -> Mp4File {
    let f = open(src);
    let o = open(out);
    assert!(o.warnings().is_empty(), "{:?}", o.warnings());
    assert_eq!(o.other_tracks(), 0);
    assert_eq!(o.codec(), f.codec());
    assert_eq!(o.index().info(), f.index().info());
    let want: Vec<_> = plans
        .iter()
        .flat_map(|p| payloads(&f, src, p.packets.clone()))
        .collect();
    let got = payloads(&o, out, 0..o.index().packets().len());
    assert_eq!(got.len(), want.len(), "sample count");
    assert!(got == want, "sample payloads differ");
    let len: u64 = plans.iter().map(CutPlan::output_len).sum();
    assert_eq!(o.index().total_samples(), len, "length");
    o
}

/// Outputs shorter than this (2 ms) are not compared with ffmpeg: ffmpeg
/// 6.1 drops a file's whole output when its edit list keeps less than
/// about 1.7 ms (70 samples at 44.1 kHz gave nothing, 80 the expected
/// frame). [`tiny_output_is_written`] keeps them covered by oracle 1.
fn ffmpeg_min_output(rate: u32) -> u64 {
    u64::from(rate) / 500
}

/// Checks oracles 2 and 3.
fn assert_decodes_like_source(name: &str, src: &[u8], out: &[u8], plans: &[CutPlan]) {
    let f = open(src);
    let o = open(out);
    if o.index().total_samples() < ffmpeg_min_output(f.index().info().sample_rate) {
        return;
    }
    let frame = 4 * usize::from(f.index().info().channels);
    let src_path = temp_file(&format!("src-{name}"), src);
    let out_path = temp_file(&format!("out-{name}"), out);
    let (Some(s), Some(d)) = (
        ffmpeg_decode(&src_path, "f32le"),
        ffmpeg_decode(&out_path, "f32le"),
    ) else {
        return;
    };
    let total: u64 = plans.iter().map(CutPlan::output_len).sum();
    // Oracle 3: older ffmpeg plays the hidden tail of the last sample as
    // well, newer ffmpeg stops where the edit list ends.
    let tail = o.index().packets().last().map_or(0, |p| p.hidden_after);
    let decoded = (d.len() / frame) as u64;
    assert!(
        decoded == total || decoded == total + u64::from(tail),
        "{name}: decoded length {decoded}, expected {total} or {total} + {tail}"
    );
    assert!(u64::from(tail) < max_frame(&f), "{name}: tail {tail}");
    assert!(
        s.len() / frame >= f.index().total_samples() as usize,
        "{name}: source length"
    );

    let mut out_pos = 0usize;
    let mut prev_end = 0u64;
    let mut window_end = 0usize;
    for p in plans {
        let r = p.actual.start as usize..p.actual.end as usize;
        out_pos += p.padding_before as usize;
        if r.start as u64 != prev_end || p.padding_before > 0 {
            window_end = r.start + join_window(&f);
        }
        let a = window_end.max(r.start).min(r.end);
        if a < r.end {
            let want = &s[a * frame..r.end * frame];
            let got_at = out_pos + (a - r.start);
            let got = &d[got_at * frame..(got_at + r.end - a) * frame];
            if let Some(i) = (0..r.end - a)
                .find(|&i| want[i * frame..(i + 1) * frame] != got[i * frame..(i + 1) * frame])
            {
                panic!(
                    "{name}: range {r:?} differs at source sample {} ({i} samples after the allowed window)",
                    a + i
                );
            }
        }
        out_pos += r.len() + p.padding_after as usize;
        prev_end = p.actual.end;
    }
    let _ = std::fs::remove_file(src_path);
    let _ = std::fs::remove_file(out_path);
}

// ------------------------------------------------------------ demuxing

#[test]
fn fixtures_open_cleanly() {
    for (name, rate, channels, len) in FIXTURES {
        let f = open(&fixture(name));
        let others = usize::from(name.contains("video"));
        assert_eq!(f.warnings().len(), others, "{name}: {:?}", f.warnings());
        assert_eq!(f.other_tracks(), others, "{name}");
        let alac = matches!(f.codec(), Mp4Codec::Alac { .. });
        assert_eq!(alac, name.contains("alac"), "{name}");
        assert_eq!(f.index().info().sample_rate, rate, "{name}");
        assert_eq!(f.index().info().channels, channels, "{name}");
        assert_eq!(f.index().total_samples(), len, "{name}");
    }
}

/// Our timeline is ffmpeg's decode without the hidden end: the same
/// start (both honour the edit list's media time) and less than one frame
/// shorter.
#[test]
fn timeline_agrees_with_ffmpeg() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let path = temp_file(name, &src);
        let Some(d) = ffmpeg_decode(&path, "f32le") else {
            return;
        };
        let frames = (d.len() / (4 * usize::from(f.index().info().channels))) as u64;
        let len = f.index().total_samples();
        assert!(
            len <= frames && frames < len + max_frame(&f),
            "{name}: {frames} vs {len}"
        );
    }
}

#[test]
fn tags_are_read() {
    let f = open(&fixture("mp4_aac_stereo_44k.m4a"));
    assert_eq!(f.tags().title.as_deref(), Some("Fixture"));
    assert_eq!(f.tags().artist.as_deref(), Some("losslessaudiocut"));
}

#[test]
fn truncated_and_damaged_files_do_not_panic() {
    for name in ["mp4_aac_stereo_44k.m4a", "mp4_aac_mono_48k.m4a"] {
        let src = fixture(name);
        let full = open(&src).index().total_samples();
        for cut in [8, 30, src.len() / 3, src.len() / 2, src.len() - 100] {
            if let Ok(f) = Mp4File::open(&mut Cursor::new(&src[..cut])) {
                // Only the faststart file still has its sample table.
                assert!(f.index().total_samples() < full, "{name} {cut}");
                assert!(!f.warnings().is_empty(), "{name} {cut}");
            }
        }
        for seed in 0..20 {
            let mut d = src.clone();
            let noise = common::noise(64, seed);
            let at = (seed as usize * 997) % (d.len() - 64);
            d[at..at + 64].copy_from_slice(&noise);
            if let Ok(f) = Mp4File::open(&mut Cursor::new(&d)) {
                let p = plans(&f, &[0..f.index().total_samples()]);
                let mut out = Cursor::new(Vec::new());
                let _ = f.write_cut(&mut Cursor::new(&d), &p, None, &mut out);
            }
        }
    }
    // moov after mdat: a file cut short loses it, which is an error.
    let src = fixture("mp4_aac_stereo_44k.m4a");
    assert!(Mp4File::open(&mut Cursor::new(&src[..src.len() / 2])).is_err());
}

// ------------------------------------------------------------ cutting

#[test]
fn whole_file_round_trips() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let p = plans(&f, &[0..f.index().total_samples()]);
        let out = write(&src, &f, &p);
        let o = assert_samples_copied(&src, &out, &p);
        assert_eq!(o.tags(), f.tags(), "{name}");
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// Start and end of a single kept range are sample-exact, wherever they
/// fall inside a sample.
#[test]
fn single_range_is_sample_exact() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        for r in [len / 5..len * 4 / 5, 1..len - 1, 0..1_234, 3_001..len, 5..9] {
            let p = plans(&f, std::slice::from_ref(&r));
            assert!(p[0].is_exact(&r), "{name}: {r:?} {p:?}");
            let out = write(&src, &f, &p);
            assert_samples_copied(&src, &out, &p);
            assert_decodes_like_source(name, &src, &out, &p);
        }
    }
}

/// Deletes join the remaining parts in one stream; the file's start and
/// end stay sample-exact, the joins are sample-granular.
#[test]
fn deletes_join_in_one_stream() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let p = delete(&f, &[(0, 777), (len / 6, len / 3), (len / 2, len * 2 / 3)]);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].actual.start, 777, "{name}");
        assert_eq!(p[2].actual.end, len, "{name}");
        let out = write(&src, &f, &p);
        assert_samples_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// The source's last sample holds encoder padding. Kept before a join it
/// cannot be trimmed, so the padding plays there and the plan says so.
#[test]
fn padding_of_the_last_sample_plays_at_a_join() {
    let name = "mp4_aac_stereo_44k.m4a";
    let src = fixture(name);
    let f = open(&src);
    let len = f.index().total_samples();
    let p = plans(&f, &[len - 100..len, 0..5_000]);
    assert!(p[0].padding_after > 0, "{p:?}");
    let out = write(&src, &f, &p);
    assert_samples_copied(&src, &out, &p);
    assert_decodes_like_source(name, &src, &out, &p);
}

/// With PNS, oracle 2 cannot hold bit for bit (see the module
/// documentation), but oracles 1 and 3 do, and the decoded signal stays
/// the source's: the difference has far less energy than the signal.
#[test]
fn noise_substitution_differs_only_in_noise() {
    let name = "mp4_aac_pns_44k.m4a";
    let src = fixture(name);
    let f = open(&src);
    let len = f.index().total_samples();
    let r = len / 4..len * 3 / 4;
    let p = plans(&f, std::slice::from_ref(&r));
    let out = write(&src, &f, &p);
    assert_samples_copied(&src, &out, &p);
    let (Some(s), Some(d)) = (
        ffmpeg_decode(&temp_file("src-pns.m4a", &src), "f32le"),
        ffmpeg_decode(&temp_file("out-pns.m4a", &out), "f32le"),
    ) else {
        return;
    };
    let f32s = |b: &[u8]| -> Vec<f32> {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|&x| f32::from_le_bytes(x))
            .collect()
    };
    let (s, d) = (f32s(&s), f32s(&d));
    let n = (r.end - r.start) as usize * 2;
    let want = &s[r.start as usize * 2..r.start as usize * 2 + n];
    let got = &d[..n];
    let energy = |x: &mut dyn Iterator<Item = f32>| x.map(|v| f64::from(v * v)).sum::<f64>();
    let signal = energy(&mut want.iter().copied());
    let diff = energy(&mut want.iter().zip(got).map(|(a, b)| a - b));
    assert!(
        diff > 0.0,
        "PNS no longer differs: make this fixture bit-exact"
    );
    // The fixture's noise bands carry about 1 % of the energy.
    assert!(diff < signal / 30.0, "{diff} vs {signal}");
}

/// A few samples from the middle of one AAC frame: our demuxer reads back
/// exactly them (see [`ffmpeg_min_output`] for why ffmpeg is not asked).
#[test]
fn tiny_output_is_written() {
    let src = fixture("mp4_aac_stereo_44k.m4a");
    let f = open(&src);
    for r in [5..9, 3_000..3_004, 3_000..3_070] {
        let p = plans(&f, std::slice::from_ref(&r));
        let out = write(&src, &f, &p);
        let o = assert_samples_copied(&src, &out, &p);
        assert_eq!(o.index().total_samples(), r.end - r.start);
    }
}

#[test]
fn trims_at_inner_joins_are_rejected() {
    let src = fixture("mp4_aac_stereo_44k.m4a");
    let f = open(&src);
    let mut p = plans(&f, &[100..20_000, 30_000..40_000]);
    p[1].trim_start = 5;
    let mut out = Cursor::new(Vec::new());
    assert!(
        f.write_cut(&mut Cursor::new(&src), &p, None, &mut out)
            .is_err()
    );
}

#[test]
fn cut_of_cut_is_stable() {
    let src = fixture("mp4_aac_stereo_44k.m4a");
    let f = open(&src);
    let len = f.index().total_samples();
    // Deletes from the end first, so the positions stay source positions.
    let p = delete(&f, &[(len - 77, len), (len / 3, len / 2), (0, 1_001)]);
    let once = write(&src, &f, &p);
    let o = open(&once);
    let again = write(&once, &o, &plans(&o, &[0..o.index().total_samples()]));
    // The second cut drops the pre-roll the first one kept beyond what
    // AAC needs, so compare what it plays, not the bytes.
    let a = open(&again);
    assert_eq!(a.index().total_samples(), o.index().total_samples());
    let n = o.index().packets().len();
    assert_eq!(payloads(&a, &again, 0..n), payloads(&o, &once, 0..n));
}

#[test]
fn empty_cut_is_a_valid_empty_file() {
    let src = fixture("mp4_aac_stereo_44k.m4a");
    let f = open(&src);
    let out = write(&src, &f, &plans(&f, &[100..100]));
    let o = open(&out);
    assert_eq!(o.index().total_samples(), 0);
}

/// `lac_container::open` recognises MP4 by its `ftyp` box.
#[test]
fn format_probe_finds_mp4() {
    for (name, ..) in FIXTURES {
        let c = lac_container::open(&mut Cursor::new(fixture(name))).expect("opens");
        assert_eq!(c.format_name(), "MP4", "{name}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Random delete sequences on every fixture: samples are copied
    /// verbatim, the output has the planned length, and (where ffmpeg is
    /// installed) decodes like the source outside the join windows.
    #[test]
    fn random_deletes_copy_samples(
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
        assert_samples_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// Noise substitution is found in the fixture encoded with it, and only
/// there; the report notes it for any cut.
#[test]
fn noise_substitution_is_detected() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let pns = f
            .uses_noise_substitution(&mut Cursor::new(&src))
            .expect("reads");
        assert!(!pns, "{name}");
    }
    for name in ["mp4_aac_pns_mono_44k.m4a", "mp4_aac_pns_44k.m4a"] {
        let src = fixture(name);
        let f = open(&src);
        let pns = f
            .uses_noise_substitution(&mut Cursor::new(&src))
            .expect("reads");
        assert!(pns, "{name}");
    }
    let src = fixture("mp4_aac_pns_44k.m4a");
    let c = lac_container::open(&mut Cursor::new(&src)).expect("opens");
    let notes = c.cut_notes(&mut Cursor::new(&src)).expect("reads");
    assert_eq!(notes, vec![lac_container::mp4::aac::NOISE_NOTE.to_owned()]);
}

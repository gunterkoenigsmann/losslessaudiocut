//! Tests of the ADTS (raw AAC) demuxer and muxer.
//!
//! Fixtures come from `tests/fixtures/generate.sh` (ffmpeg's native AAC
//! encoder: stereo at 44.1 kHz and mono at 22.05 kHz without perceptual
//! noise substitution, and one stream with it).
//!
//! Oracles:
//! 1. Frames are copied verbatim: the output is the kept frames (pre-roll
//!    frames included), byte for byte, and re-opening it yields the
//!    planned length.
//! 2. ffmpeg (an independent demuxer and decoder) decodes the output like
//!    the source restricted to the plans' actual ranges, bit for bit,
//!    except for the first [`JOIN_WINDOW`] samples of the output (unless
//!    it starts at the source's start) and after each join: that is the
//!    pre-roll frame, whose inverse MDCT is overlapped with a different
//!    predecessor there (ISO/IEC 14496-3 4.6.11). Every wanted frame after
//!    it decodes exactly.

// Lists of kept ranges often hold a single range here.
#![allow(clippy::single_range_in_vec_init)]

mod common;

use std::io::Cursor;
use std::ops::Range;

use common::{ffmpeg_decode_with, fixture, temp_file};
use lac_container::adts::{AdtsFile, FRAME_SAMPLES};
use lac_container::mp4::aac::NOISE_NOTE;
use lac_container::{ContainerError, tags::id3};
use lac_core::{CutPlan, EditList, SourceId, Tags, plan_cuts};
use proptest::prelude::*;

/// Fixtures without noise substitution: (name, sample rate, channels).
const FIXTURES: [(&str, u32, u16); 2] = [
    ("adts_aac_stereo_44k.aac", 44_100, 2),
    ("adts_aac_mono_22k.aac", 22_050, 1),
];

/// Samples at the start of the output and after each join that may
/// differ from the source: the pre-roll frame.
const JOIN_WINDOW: usize = FRAME_SAMPLES as usize;

/// The input format: ffmpeg's probe does not recognise a file of only one
/// or two frames.
const FFMPEG_OPTS: [&str; 2] = ["-f", "aac"];

fn open(bytes: &[u8]) -> AdtsFile {
    AdtsFile::open(&mut Cursor::new(bytes)).expect("valid ADTS")
}

fn write(src: &[u8], f: &AdtsFile, plans: &[CutPlan], tags: Option<&Tags>) -> Vec<u8> {
    let mut out = Vec::new();
    f.write_cut(&mut Cursor::new(src), plans, tags, &mut out)
        .expect("write");
    out
}

fn plans(f: &AdtsFile, ranges: &[Range<u64>]) -> Vec<CutPlan> {
    plan_cuts(f.index(), f.capabilities(), ranges).expect("plan")
}

/// Deletes timeline ranges one after the other, like the editor does, and
/// returns the plans of the remaining source ranges.
fn delete(f: &AdtsFile, deletes: &[(u64, u64)]) -> Vec<CutPlan> {
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

/// Checks oracle 1.
fn assert_frames_copied(src: &[u8], out: &[u8], plans: &[CutPlan]) {
    let f = open(src);
    let mut want = Vec::new();
    for p in plans {
        for pk in &f.index().packets()[p.packets.clone()] {
            let at = pk.byte_offset as usize;
            want.extend_from_slice(&src[at..at + pk.byte_len as usize]);
        }
    }
    assert!(out == want, "output is not the kept frames");
    let o = open(out);
    assert!(o.warnings().is_empty(), "{:?}", o.warnings());
    assert_eq!(o.params(), f.params());
    let len: u64 = plans.iter().map(CutPlan::output_len).sum();
    assert_eq!(o.index().total_samples(), len, "length");
}

fn ffmpeg_f32(name: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    let path = temp_file(name, bytes);
    let d = ffmpeg_decode_with(&path, "f32le", &FFMPEG_OPTS);
    let _ = std::fs::remove_file(path);
    d
}

/// Checks oracle 2.
fn assert_decodes_like_source(name: &str, src: &[u8], out: &[u8], plans: &[CutPlan]) {
    let f = open(src);
    let frame = 4 * usize::from(f.index().info().channels);
    let (Some(s), Some(d)) = (
        ffmpeg_f32(&format!("src-{name}"), src),
        ffmpeg_f32(&format!("out-{name}"), out),
    ) else {
        return;
    };
    let total: u64 = plans.iter().map(CutPlan::output_len).sum();
    assert_eq!((d.len() / frame) as u64, total, "{name}: decoded length");

    let mut out_pos = 0usize;
    // The decoder starts without a predecessor: like a join, unless the
    // output starts where the source does.
    let mut prev_end = 0u64;
    for p in plans {
        let r = p.actual.start as usize..p.actual.end as usize;
        let window = if r.start as u64 == prev_end {
            0
        } else {
            JOIN_WINDOW
        };
        let a = (r.start + window).min(r.end);
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
        out_pos += r.len();
        prev_end = p.actual.end;
    }
}

// ------------------------------------------------------------ demuxing

#[test]
fn fixtures_open_cleanly() {
    for (name, rate, channels) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        assert!(f.warnings().is_empty(), "{name}: {:?}", f.warnings());
        assert_eq!(f.index().info().sample_rate, rate, "{name}");
        assert_eq!(f.index().info().channels, channels, "{name}");
        assert_eq!(f.params().profile_name(), "AAC LC", "{name}");
        // ffmpeg decodes exactly our timeline: whole frames, no delay.
        if let Some(d) = ffmpeg_f32(name, &src) {
            let frames = d.len() / (4 * usize::from(channels));
            assert_eq!(frames as u64, f.index().total_samples(), "{name}");
        }
    }
}

#[test]
fn detected_with_and_without_id3() {
    let src = fixture("adts_aac_stereo_44k.aac");
    let c = lac_container::open(&mut Cursor::new(&src)).expect("opens");
    assert_eq!(c.format_name(), "ADTS");
    assert_eq!(c.details(), "AAC LC");
    let tags = Tags {
        title: Some("Tïtle".into()),
        ..Tags::default()
    };
    let f = open(&src);
    let all = plans(&f, &[0..f.index().total_samples()]);
    let tagged = write(&src, &f, &all, Some(&tags));
    let c = lac_container::open(&mut Cursor::new(&tagged)).expect("opens");
    assert_eq!(c.format_name(), "ADTS");
    assert_eq!(c.tags(), &tags);
    assert_eq!(c.index().total_samples(), f.index().total_samples());
    // MP3 is still MP3, although its 11-bit sync word matches ADTS's start.
    let mp3 = fixture("mp3_cbr128_44k.mp3");
    let c = lac_container::open(&mut Cursor::new(&mp3)).expect("opens");
    assert_eq!(c.format_name(), "MP3");
}

#[test]
fn tags_survive_cuts_or_are_replaced() {
    let src = fixture("adts_aac_mono_22k.aac");
    let f = open(&src);
    let all = plans(&f, &[0..f.index().total_samples()]);
    // Without a source tag and without new tags, nothing is written.
    assert_eq!(write(&src, &f, &all, None), src);
    assert_eq!(write(&src, &f, &all, Some(&Tags::default())), src);

    let tags = Tags {
        artist: Some("A".into()),
        track_number: Some(2),
        ..Tags::default()
    };
    let tagged = write(&src, &f, &all, Some(&tags));
    let t = open(&tagged);
    assert_eq!(t.tags(), &tags);
    // The source's tag is copied verbatim into a cut.
    let len = t.index().total_samples();
    let p = plans(&t, &[len / 4..len / 2]);
    let out = write(&tagged, &t, &p, None);
    let tag_len = id3::tag_len(&tagged).expect("ID3 tag");
    assert_eq!(out[..tag_len], tagged[..tag_len]);
    assert_eq!(open(&out).tags(), &tags);
}

#[test]
fn junk_and_truncation_are_tolerated() {
    let src = fixture("adts_aac_mono_22k.aac");
    let f = open(&src);
    let n = f.index().packets().len();
    let p2 = f.index().packets()[2];
    // Junk between frames 1 and 2 (without a sync word in it): frame 1 is
    // not confirmed by a following header, so it is lost with the junk.
    let mut j = src[..p2.byte_offset as usize].to_vec();
    j.extend([0u8; 37]);
    j.extend(&src[p2.byte_offset as usize..]);
    let g = open(&j);
    assert_eq!(g.index().packets().len(), n - 1);
    assert_eq!(g.warnings().len(), 1, "{:?}", g.warnings());
    // A truncated last frame is dropped.
    let t = open(&src[..src.len() - 10]);
    assert_eq!(t.index().packets().len(), n - 1);
    assert_eq!(t.warnings().len(), 1);
    // Damage never panics.
    for seed in 0..30 {
        let mut d = src.clone();
        let noise = common::noise(40, seed);
        let at = (seed as usize * 131) % (d.len() - 40);
        d[at..at + 40].copy_from_slice(&noise);
        if let Ok(f) = AdtsFile::open(&mut Cursor::new(&d)) {
            let p = plans(&f, &[0..f.index().total_samples()]);
            let _ = f.write_cut(&mut Cursor::new(&d), &p, None, &mut Vec::new());
            let _ = f.uses_noise_substitution(&mut Cursor::new(&d));
        }
    }
    assert!(AdtsFile::open(&mut Cursor::new(&[0u8; 100][..])).is_err());
}

#[test]
fn channel_configuration_zero_is_refused() {
    let mut src = fixture("adts_aac_stereo_44k.aac");
    // Clear the channel configuration of every header (bit 0 of byte 2
    // and bits 7..6 of byte 3); all headers of a stream agree on it.
    for p in open(&src.clone()).index().packets() {
        let at = p.byte_offset as usize;
        src[at + 2] &= !1;
        src[at + 3] &= 0x3F;
    }
    match AdtsFile::open(&mut Cursor::new(&src)) {
        Err(ContainerError::Unsupported(m)) => assert!(m.contains("configuration 0"), "{m}"),
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------ cutting

#[test]
fn whole_file_round_trips() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let p = plans(&f, &[0..f.index().total_samples()]);
        let out = write(&src, &f, &p, None);
        assert_eq!(out, src, "{name}");
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// Cuts are frame-granular: a single range grows to whole frames, plus
/// one pre-roll frame in front unless it starts in the first frame.
#[test]
fn single_range_is_frame_granular() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let fs = u64::from(FRAME_SAMPLES);
        for r in [len / 5..len * 4 / 5, 1..len - 1, 0..fs, fs..2 * fs, 5..9] {
            let p = plans(&f, std::slice::from_ref(&r));
            let a = &p[0].actual;
            assert!(a.start <= r.start && r.end <= a.end, "{name}: {r:?} {p:?}");
            assert_eq!((a.start % fs, a.end % fs), (0, 0), "{name}: {r:?}");
            let first = r.start / fs * fs;
            assert_eq!(a.start, first.saturating_sub(fs), "{name}: {r:?} pre-roll");
            let out = write(&src, &f, &p, None);
            assert_frames_copied(&src, &out, &p);
            assert_decodes_like_source(name, &src, &out, &p);
        }
    }
}

#[test]
fn deletes_join_in_one_stream() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let p = delete(&f, &[(0, 2000), (len / 6, len / 3), (len / 2, len * 2 / 3)]);
        assert_eq!(p.len(), 3, "{name}");
        let out = write(&src, &f, &p, None);
        assert_frames_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn random_deletes(
        which in 0..FIXTURES.len(),
        cuts in proptest::collection::vec((0.0..1.0f64, 0.0..0.3f64), 1..4),
    ) {
        let (name, ..) = FIXTURES[which];
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let mut deletes = Vec::new();
        let mut left = len;
        for (at, size) in cuts {
            let a = (at * left as f64) as u64;
            let b = (a + (size * left as f64) as u64 + 1).min(left);
            if b - a < left {
                deletes.push((a, b));
                left -= b - a;
            }
        }
        let p = delete(&f, &deletes);
        prop_assume!(!p.is_empty());
        let out = write(&src, &f, &p, None);
        assert_frames_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// Noise substitution is found where the encoder used it, and only there.
#[test]
fn noise_substitution_is_detected() {
    for (name, uses) in [
        ("adts_aac_pns_44k.aac", true),
        ("adts_aac_stereo_44k.aac", false),
        ("adts_aac_mono_22k.aac", false),
    ] {
        let src = fixture(name);
        let c = lac_container::open(&mut Cursor::new(&src)).expect("opens");
        let notes = c.cut_notes(&mut Cursor::new(&src)).expect("reads");
        let want: Vec<String> = if uses {
            vec![NOISE_NOTE.to_owned()]
        } else {
            Vec::new()
        };
        assert_eq!(notes, want, "{name}");
    }
}

/// Tracks in another order than the source's (issue #85): ranges written
/// out of order join like deletes, with the same window at each join.
#[test]
fn ranges_out_of_order_join_like_deletes() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let p = plans(&f, &[len * 2 / 3..len, len / 3..len / 2, 2000..len / 6]);
        let out = write(&src, &f, &p, None);
        assert_frames_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

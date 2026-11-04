//! Tests of the raw AC-3 demuxer and muxer.
//!
//! Fixtures come from `tests/fixtures/generate.sh` (ffmpeg's AC-3
//! encoder: stereo at 44.1 kHz, 5.1 at 48 kHz, mono at 32 kHz).
//!
//! Oracles:
//! 1. Frames are copied verbatim: the output is the kept frames, byte for
//!    byte, and re-opening it yields the planned length.
//! 2. ffmpeg (an independent demuxer and decoder) decodes the output like
//!    the source restricted to the plans' actual ranges, bit for bit,
//!    except for the first [`JOIN_WINDOW`] samples of the output (unless
//!    it starts at the source's start) and after each join: the first
//!    block of a frame is overlapped with the last block of the frame
//!    before it (A/52 7.9.4.2), which is a different one there.
//!
//! ffmpeg runs with `-cons_noisegen 1`, as `lac-decode` does: otherwise its
//! dither generator (A/52 7.3.4) runs from the start of the stream, and
//! mantissas without bits decode to different noise after any cut.

// Lists of kept ranges often hold a single range here.
#![allow(clippy::single_range_in_vec_init)]

mod common;

use std::io::Cursor;
use std::ops::Range;

use common::{ffmpeg_decode_with, fixture, temp_file};
use lac_container::ac3::{self, Ac3File, FRAME_SAMPLES};
use lac_container::{ContainerError, tags::id3};
use lac_core::{CutPlan, EditList, SourceId, Tags, plan_cuts};
use proptest::prelude::*;

/// Fixtures: (name, sample rate, channels, Dolby channel arrangement).
const FIXTURES: [(&str, u32, u16, &str); 3] = [
    ("ac3_stereo_44k.ac3", 44_100, 2, "2/0"),
    ("ac3_51_48k.ac3", 48_000, 6, "3/2.1"),
    ("ac3_mono_32k.ac3", 32_000, 1, "1/0"),
];

/// Samples at the start of the output and after each join that may
/// differ from the source: the first of a frame's six blocks.
const JOIN_WINDOW: usize = 256;

/// Decoder options that make ffmpeg's AC-3 output independent of where
/// decoding started (see the module documentation), and the input format:
/// ffmpeg's probe does not recognise a file of only one or two frames.
const FFMPEG_OPTS: [&str; 4] = ["-cons_noisegen", "1", "-f", "ac3"];

fn open(bytes: &[u8]) -> Ac3File {
    Ac3File::open(&mut Cursor::new(bytes)).expect("valid AC-3")
}

fn write(src: &[u8], f: &Ac3File, plans: &[CutPlan], tags: Option<&Tags>) -> Vec<u8> {
    let mut out = Vec::new();
    f.write_cut(&mut Cursor::new(src), plans, tags, &mut out)
        .expect("write");
    out
}

fn plans(f: &Ac3File, ranges: &[Range<u64>]) -> Vec<CutPlan> {
    plan_cuts(f.index(), f.capabilities(), ranges).expect("plan")
}

/// Deletes timeline ranges one after the other, like the editor does, and
/// returns the plans of the remaining source ranges.
fn delete(f: &Ac3File, deletes: &[(u64, u64)]) -> Vec<CutPlan> {
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
    for (name, rate, channels, layout) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        assert!(f.warnings().is_empty(), "{name}: {:?}", f.warnings());
        assert_eq!(f.index().info().sample_rate, rate, "{name}");
        assert_eq!(f.index().info().channels, channels, "{name}");
        assert_eq!(f.params().layout(), layout, "{name}");
        // Every frame carries a valid crc2, which checks our CRC against
        // a real encoder.
        for p in f.index().packets() {
            let at = p.byte_offset as usize;
            assert!(ac3::crc_ok(&src[at..at + p.byte_len as usize]), "{name}");
        }
        // ffmpeg decodes exactly our timeline: whole frames, no delay.
        if let Some(d) = ffmpeg_f32(name, &src) {
            let frames = d.len() / (4 * usize::from(channels));
            assert_eq!(frames as u64, f.index().total_samples(), "{name}");
        }
    }
    // 44.1 kHz streams pad every other frame by one word.
    let f = open(&fixture("ac3_stereo_44k.ac3"));
    let mut sizes: Vec<_> = f.index().packets().iter().map(|p| p.byte_len).collect();
    sizes.sort_unstable();
    sizes.dedup();
    assert_eq!(sizes, vec![834, 836]); // 192 kbit/s
}

#[test]
fn eac3_is_rejected() {
    let mut src = fixture("ac3_stereo_44k.ac3");
    src[5] = (src[5] & 7) | (16 << 3); // bsid 16
    match Ac3File::open(&mut Cursor::new(&src)) {
        Err(ContainerError::Unsupported(m)) => assert!(m.contains("E-AC-3"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn detected_with_and_without_id3() {
    let src = fixture("ac3_51_48k.ac3");
    let c = lac_container::open(&mut Cursor::new(&src)).expect("opens");
    assert_eq!(c.format_name(), "AC-3");
    assert_eq!(c.details(), "3/2.1");
    let tags = Tags {
        title: Some("Tïtle".into()),
        ..Tags::default()
    };
    let f = open(&src);
    let all = plans(&f, &[0..f.index().total_samples()]);
    let tagged = write(&src, &f, &all, Some(&tags));
    let c = lac_container::open(&mut Cursor::new(&tagged)).expect("opens");
    assert_eq!(c.format_name(), "AC-3");
    assert_eq!(c.tags(), &tags);
    assert_eq!(c.index().total_samples(), f.index().total_samples());
}

#[test]
fn tags_survive_cuts_or_are_replaced() {
    let src = fixture("ac3_stereo_44k.ac3");
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
    let src = fixture("ac3_mono_32k.ac3");
    let f = open(&src);
    let n = f.index().packets().len();
    let p2 = f.index().packets()[2];
    // Junk between frames 1 and 2 (without a sync word in it).
    let mut j = src[..p2.byte_offset as usize].to_vec();
    j.extend([0u8; 37]);
    j.extend(&src[p2.byte_offset as usize..]);
    let g = open(&j);
    assert_eq!(g.index().packets().len(), n);
    assert_eq!(g.warnings().len(), 1, "{:?}", g.warnings());
    // A cut over the junk leaves it out.
    let all = plans(&g, &[0..g.index().total_samples()]);
    assert_eq!(write(&j, &g, &all, None), src);
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
        if let Ok(f) = Ac3File::open(&mut Cursor::new(&d)) {
            let p = plans(&f, &[0..f.index().total_samples()]);
            let _ = f.write_cut(&mut Cursor::new(&d), &p, None, &mut Vec::new());
        }
    }
    assert!(Ac3File::open(&mut Cursor::new(&[0u8; 100][..])).is_err());
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

/// Cuts are frame-granular: a single range grows to whole frames.
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

/// ffmpeg's encoder asks for dither, so every fixture gets the note.
#[test]
fn dither_is_detected() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        assert!(
            f.uses_dither(&mut Cursor::new(&src)).expect("reads"),
            "{name}"
        );
        let c = lac_container::open(&mut Cursor::new(&src)).expect("opens");
        let notes = c.cut_notes(&mut Cursor::new(&src)).expect("reads");
        assert_eq!(notes, vec![ac3::DITHER_NOTE.to_owned()], "{name}");
    }
}

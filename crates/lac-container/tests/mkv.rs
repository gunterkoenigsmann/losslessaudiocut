//! Tests of the Matroska/WebM demuxer and muxer (Opus).
//!
//! Fixtures come from `tests/fixtures/generate.sh` (ffmpeg with libopus;
//! a live-style file with unknown sizes, a damaged file and one with a
//! video track, derived by `mkv_mangle.py`).
//!
//! Oracles:
//! 1. Packets are copied verbatim: re-opening the output with our demuxer
//!    yields exactly the kept source packets, byte for byte, and the
//!    planned length. This holds without external tools.
//! 2. ffmpeg (an independent demuxer and decoder) decodes the output like
//!    the source restricted to the plans' actual ranges, bit for bit,
//!    except within [`JOIN_WINDOW`] after a join point: Matroska joins the
//!    parts in one stream without pre-roll, so the decoder has to converge.
//! 3. ffmpeg's output length equals the planned length, so it honours our
//!    codec delay and discard padding.

// Lists of kept ranges often hold a single range here.
#![allow(clippy::single_range_in_vec_init)]

mod common;

use std::io::Cursor;
use std::ops::Range;

use common::{ffmpeg_decode, fixture, temp_file};
use lac_container::mkv::MkvFile;
use lac_core::{CutPlan, EditList, SourceId, Tags, plan_cuts};
use proptest::prelude::*;

/// Clean fixtures: (name, DocType, channels, length in samples). The
/// lengths are exactly the encoder's input: codec delay and the final
/// discard padding are honoured.
const FIXTURES: [(&str, &str, u16, u64); 4] = [
    ("mkv_opus_stereo.webm", "webm", 2, 72_000),
    ("mkv_opus_mono_10ms.mka", "matroska", 1, 48_000),
    ("mkv_opus_live.webm", "webm", 2, 72_000),
    ("mkv_opus_with_video.mkv", "matroska", 1, 48_000),
];

/// Seconds after a join point in which the output may differ from the
/// source (see oracle 2). Without pre-roll the decoder starts the part
/// after a join from the state of the part before it; on the speech-like
/// fixtures it becomes bit-identical again within at most 330 ms (measured
/// over 36 joins; the 10 ms SILK frames of the mono fixture converge
/// slowest), as for Ogg Speex.
const JOIN_WINDOW: f64 = 0.4;

fn open(bytes: &[u8]) -> MkvFile {
    MkvFile::open(&mut Cursor::new(bytes)).expect("valid Matroska")
}

fn write(src: &[u8], f: &MkvFile, plans: &[CutPlan]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    f.write_cut(&mut Cursor::new(src), plans, None, &mut out)
        .expect("write");
    out.into_inner()
}

fn plans(f: &MkvFile, ranges: &[Range<u64>]) -> Vec<CutPlan> {
    plan_cuts(f.index(), f.capabilities(), ranges).expect("plan")
}

/// Deletes timeline ranges one after the other, like the editor does, and
/// returns the plans of the remaining source ranges.
fn delete(f: &MkvFile, deletes: &[(u64, u64)]) -> Vec<CutPlan> {
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

fn payloads(f: &MkvFile, bytes: &[u8], entries: impl Iterator<Item = usize>) -> Vec<Vec<u8>> {
    let mut r = Cursor::new(bytes);
    entries
        .map(|e| f.read_packet(&mut r, e).expect("packet"))
        .collect()
}

/// Checks oracle 1 and returns the re-opened output.
fn assert_packets_copied(src: &[u8], out: &[u8], plans: &[CutPlan]) -> MkvFile {
    let f = open(src);
    let o = open(out);
    assert!(o.warnings().is_empty(), "{:?}", o.warnings());
    assert_eq!(o.doc_type(), f.doc_type());
    assert_eq!(o.other_tracks(), 0);
    assert_eq!(o.index().info(), f.index().info());
    let want: Vec<_> = plans
        .iter()
        .flat_map(|p| payloads(&f, src, p.packets.clone()))
        .collect();
    let got = payloads(&o, out, 0..o.index().packets().len());
    assert_eq!(got.len(), want.len(), "packet count");
    assert!(got == want, "packet payloads differ");
    let len: u64 = plans.iter().map(CutPlan::output_len).sum();
    assert_eq!(o.index().total_samples(), len, "length");
    o
}

/// Whether ffmpeg decodes the output of `plans` too long: when it holds a
/// single packet whose head is hidden by the codec delay and whose tail by
/// discard padding, ffmpeg forgets the rest of the codec delay when the
/// discard padding arrives (see the `ogg` module docs; the Matroska
/// demuxer passes both the same way). [`sliver_inside_one_packet`] pins it.
fn ffmpeg_mistrims(f: &MkvFile, plans: &[CutPlan]) -> bool {
    let kept: Vec<_> = plans.iter().filter(|p| !p.packets.is_empty()).collect();
    let [p] = kept[..] else {
        return false;
    };
    let packet = &f.index().packets()[p.packets.start];
    p.packets.len() == 1
        && (p.trim_start > 0 || packet.hidden_before > 0)
        && (p.trim_end > 0 || packet.hidden_after > 0)
}

/// Checks oracles 2 and 3: ffmpeg's decode of `out` is the source's decode
/// restricted to the plans' output (actual range plus padding).
fn assert_decodes_like_source(name: &str, src: &[u8], out: &[u8], plans: &[CutPlan]) {
    let f = open(src);
    if ffmpeg_mistrims(&f, plans) {
        return;
    }
    let frame = 4 * usize::from(f.index().info().channels);
    let src_path = temp_file(&format!("src-{name}"), src);
    let out_path = temp_file(&format!("out-{name}"), out);
    let (Some(s), Some(o)) = (
        ffmpeg_decode(&src_path, "f32le"),
        ffmpeg_decode(&out_path, "f32le"),
    ) else {
        return;
    };
    let total: u64 = plans.iter().map(CutPlan::output_len).sum();
    assert_eq!(o.len() / frame, total as usize, "{name}: decoded length");
    assert_eq!(
        s.len() / frame,
        f.index().total_samples() as usize,
        "{name}: source length"
    );

    let window = (JOIN_WINDOW * 48_000.0) as usize;
    let mut out_pos = 0usize;
    let mut prev_end = 0u64;
    let mut window_end = 0usize;
    for p in plans {
        let r = p.actual.start as usize..p.actual.end as usize;
        // Padding is decoder output the source hides; nothing to compare.
        out_pos += p.padding_before as usize;
        if r.start as u64 != prev_end || p.padding_before > 0 {
            window_end = r.start + window;
        }
        let a = window_end.max(r.start).min(r.end);
        if a < r.end {
            let want = &s[a * frame..r.end * frame];
            let got_at = out_pos + (a - r.start);
            let got = &o[got_at * frame..(got_at + r.end - a) * frame];
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
    for (name, doc_type, channels, len) in FIXTURES {
        let f = open(&fixture(name));
        let others = usize::from(name.contains("video"));
        assert_eq!(f.warnings().len(), others, "{name}: {:?}", f.warnings());
        assert_eq!(f.other_tracks(), others, "{name}");
        assert_eq!(f.doc_type(), doc_type, "{name}");
        assert_eq!(f.index().info().sample_rate, 48_000, "{name}");
        assert_eq!(f.index().info().channels, channels, "{name}");
        assert_eq!(f.index().total_samples(), len, "{name}");
    }
}

/// Unknown cluster and segment sizes (live recordings) give the same
/// timeline and packets as the same audio in a seekable file.
#[test]
fn live_file_matches_seekable_one() {
    let (a, b) = (
        fixture("mkv_opus_stereo.webm"),
        fixture("mkv_opus_live.webm"),
    );
    let (fa, fb) = (open(&a), open(&b));
    let n = fa.index().packets().len();
    assert_eq!(fb.index().packets().len(), n);
    assert_eq!(payloads(&fa, &a, 0..n), payloads(&fb, &b, 0..n));
}

#[test]
fn length_agrees_with_ffmpeg() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let path = temp_file(name, &src);
        let Some(d) = ffmpeg_decode(&path, "f32le") else {
            return;
        };
        let frames = d.len() / (4 * usize::from(f.index().info().channels));
        assert_eq!(frames as u64, f.index().total_samples(), "{name}");
    }
}

#[test]
fn damaged_cluster_is_skipped_with_warning() {
    let clean = open(&fixture("mkv_opus_stereo.webm"));
    let src = fixture("mkv_opus_damaged.webm");
    let f = open(&src);
    assert_eq!(f.warnings().len(), 1, "{:?}", f.warnings());
    assert!(f.warnings()[0].contains("damaged"), "{:?}", f.warnings());
    assert!(f.index().total_samples() < clean.index().total_samples());
    let p = plans(&f, &[0..f.index().total_samples()]);
    let out = write(&src, &f, &p);
    assert_packets_copied(&src, &out, &p);
}

#[test]
fn truncated_files_open() {
    let src = fixture("mkv_opus_stereo.webm");
    let full = open(&src).index().total_samples();
    for cut in [src.len() / 3, src.len() / 2, src.len() * 3 / 4] {
        let f = open(&src[..cut]);
        assert!(f.index().total_samples() < full, "{cut}");
        assert!(
            f.warnings().iter().any(|w| w.contains("truncated")),
            "{cut}: {:?}",
            f.warnings()
        );
    }
}

#[test]
fn garbage_is_rejected_without_panic() {
    let src = fixture("mkv_opus_stereo.webm");
    for data in [
        vec![0x1A, 0x45, 0xDF, 0xA3],
        src[..200].to_vec(),
        [&src[..40], common::noise(5_000, 3).as_slice()].concat(),
        common::noise(10_000, 7),
    ] {
        let _ = MkvFile::open(&mut Cursor::new(&data));
    }
    // A file that ends before its track list has no audio.
    assert!(MkvFile::open(&mut Cursor::new(&src[..200])).is_err());
}

// ------------------------------------------------------------ cutting

#[test]
fn whole_file_round_trips() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let p = plans(&f, &[0..f.index().total_samples()]);
        let out = write(&src, &f, &p);
        assert_packets_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// Start and end of a single kept range are sample-exact, wherever they
/// fall inside a packet.
#[test]
fn single_range_is_sample_exact() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        for r in [len / 5..len * 4 / 5, 1..len - 1, 0..1_234, 30_001..len] {
            let p = plans(&f, std::slice::from_ref(&r));
            assert!(p[0].is_exact(&r), "{name}: {r:?} {p:?}");
            let out = write(&src, &f, &p);
            assert_packets_copied(&src, &out, &p);
            assert_decodes_like_source(name, &src, &out, &p);
        }
    }
}

/// Deletes join the remaining parts in one stream; the file's start and
/// end stay sample-exact, the joins are packet-granular.
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
        assert_packets_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// A range inside a single packet is written correctly (our demuxer agrees
/// on its length), but ffmpeg plays too much of the packet. How much
/// depends on the version: 6.1 forgets the codec delay once the discard
/// padding arrives (100 + 412 samples here), newer versions on Windows CI
/// apply the codec delay but drop the discard padding (960 - 412). Both lie
/// within the one packet. This test documents that and fails once ffmpeg
/// decodes exactly the 100 samples, so [`ffmpeg_mistrims`] can go.
#[test]
fn sliver_inside_one_packet() {
    let name = "mkv_opus_stereo.webm";
    let src = fixture(name);
    let f = open(&src);
    // The first packet holds source samples 0..648 (after 312 samples of
    // codec delay).
    let p = plans(&f, &[100..200]);
    assert!(ffmpeg_mistrims(&f, &p));
    let out = write(&src, &f, &p);
    assert_packets_copied(&src, &out, &p);
    let path = temp_file(&format!("sliver-{name}"), &out);
    if let Some(d) = ffmpeg_decode(&path, "f32le") {
        let frames = d.len() / 8;
        assert!(100 < frames && frames <= 960, "{name}: {frames}");
    }
}

#[test]
fn trims_at_inner_joins_are_rejected() {
    let src = fixture("mkv_opus_stereo.webm");
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
    let src = fixture("mkv_opus_stereo.webm");
    let f = open(&src);
    let len = f.index().total_samples();
    // Deletes from the end first, so the positions stay source positions.
    let p = delete(&f, &[(len - 77, len), (len / 3, len / 2), (0, 1_001)]);
    let once = write(&src, &f, &p);
    let o = open(&once);
    let again = write(&once, &o, &plans(&o, &[0..o.index().total_samples()]));
    assert_eq!(once, again);
}

#[test]
fn empty_cut_writes_nothing() {
    let src = fixture("mkv_opus_stereo.webm");
    let f = open(&src);
    assert!(write(&src, &f, &plans(&f, &[100..100])).is_empty());
}

/// `lac_container::open` recognises the EBML header.
#[test]
fn format_probe_finds_matroska() {
    for (name, ..) in FIXTURES {
        let c = lac_container::open(&mut Cursor::new(fixture(name))).expect("opens");
        let want = if name.ends_with(".webm") {
            "WebM"
        } else {
            "Matroska"
        };
        assert_eq!(c.format_name(), want, "{name}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Random delete sequences on every fixture: packets are copied
    /// verbatim, the output has the planned length, and (where ffmpeg is
    /// installed) decodes like the source outside the join windows.
    #[test]
    fn random_deletes_copy_packets(
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
        assert_packets_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

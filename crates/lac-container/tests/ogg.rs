//! Tests of the Ogg demuxer and muxer (Vorbis, Opus, Speex).
//!
//! Fixtures come from `tests/fixtures/generate.sh` (ffmpeg with libvorbis,
//! libopus and libspeex, chained files, and files damaged the way radio
//! recordings are).
//!
//! Oracles:
//! 1. Packets are copied verbatim: re-opening the output with our demuxer
//!    yields exactly the kept source packets, byte for byte. This holds
//!    without external tools and is checked by property tests.
//! 2. An independent decoder decodes output and source: the reference
//!    decoder `oggdec` (libvorbisfile) for Vorbis, ffmpeg for Opus and
//!    Speex. Vorbis must match bit for bit, also across join points (every
//!    run of kept packets becomes a chained stream that starts with its
//!    predecessor packet as pre-roll). Opus must match bit for bit except
//!    within [`OPUS_JOIN_WINDOW`] after a join point, where its predictive
//!    decoder converges. Speex must match bit for bit up to the first join;
//!    after a join anything goes for [`SPEEX_JOIN_WINDOW`], and from then on
//!    the difference must stay below [`SPEEX_JOIN_TOLERANCE`] (issue #17).
//! 3. That decoder's output length equals the planned length, so an
//!    independent demuxer agrees with our granule positions.

// Lists of kept ranges often hold a single range here.
#![allow(clippy::single_range_in_vec_init)]

mod common;

use std::io::Cursor;
use std::ops::Range;

use common::{ffmpeg_decode, fixture, oggdec_decode, temp_file};
use lac_container::ogg::OggFile;
use lac_container::ogg::codec::CodecKind;
use lac_core::{CutPlan, EditList, SourceId, Tags, plan_cut};
use proptest::prelude::*;

/// Single-link fixtures: (name, codec, rate, channels, length in samples).
///
/// Lengths are computed from packet contents. Vorbis and Opus honour the
/// end trim of the final granule position (Vorbis I A.2, RFC 7845 4.4), so
/// their lengths are exactly the encoder's input; for Speex they include
/// the source's end padding that a granule-trusting player would cut off.
const FIXTURES: [(&str, CodecKind, u32, u16, u64); 4] = [
    (
        "ogg_vorbis_stereo_44k.ogg",
        CodecKind::Vorbis,
        44_100,
        2,
        66_150,
    ),
    (
        "ogg_vorbis_mono_22k.ogg",
        CodecKind::Vorbis,
        22_050,
        1,
        22_050,
    ),
    ("ogg_opus_stereo.opus", CodecKind::Opus, 48_000, 2, 72_000),
    ("ogg_speex_16k.spx", CodecKind::Speex, 16_000, 1, 16_000),
];

/// Seconds after a join point in which Opus output may differ from the
/// source. Each Opus part is a chained stream with its own pre-roll, so the
/// decoder only has to converge from a nearby state.
const OPUS_JOIN_WINDOW: f64 = 0.25;

/// Seconds after a join point in which Speex output may differ from the
/// source. Speex parts are joined inside one stream without pre-roll, and
/// the decoder's float synthesis-filter and excitation state survives the
/// 80 ms pauses of the speech-like fixture (`synth_wav.py speech`): it only
/// becomes bit-identical somewhere in the second syllable after the join.
/// Measured worst case over 60 joins: 320 ms (see `docs/architecture.md`,
/// "Join points inside a file").
const SPEEX_JOIN_WINDOW: f64 = 0.4;

/// Largest difference allowed after [`SPEEX_JOIN_WINDOW`]: one 16-bit LSB
/// (2^-15), Speex's native output resolution. Bit-exact equality is not a
/// fixed-time property for Speex: the float decoder state decays
/// geometrically after a join (below 1e-6 after about 200 ms on the
/// fixture) and becomes bit-identical only when the audio lets it, which on
/// continuous signals may never happen (issue #17). Wrong packets or offsets
/// produce differences many orders of magnitude larger.
const SPEEX_JOIN_TOLERANCE: f32 = 1.0 / 32768.0;

fn open(bytes: &[u8]) -> OggFile {
    OggFile::open(&mut Cursor::new(bytes)).expect("valid Ogg")
}

fn write(src: &[u8], f: &OggFile, plans: &[CutPlan]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    f.write_cut(&mut Cursor::new(src), plans, None, &mut out)
        .expect("write");
    out.into_inner()
}

fn plans(f: &OggFile, ranges: &[Range<u64>]) -> Vec<CutPlan> {
    ranges
        .iter()
        .map(|r| plan_cut(f.index(), f.capabilities(), r.clone()).expect("plan"))
        .collect()
}

/// Deletes timeline ranges one after the other, like the editor does, and
/// returns the plans of the remaining source ranges.
fn delete(f: &OggFile, deletes: &[(u64, u64)]) -> Vec<CutPlan> {
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

/// All payloads of the given index entries.
fn payloads(f: &OggFile, bytes: &[u8], entries: impl Iterator<Item = usize>) -> Vec<Vec<u8>> {
    let mut r = Cursor::new(bytes);
    entries
        .map(|e| f.read_packet(&mut r, e).expect("packet"))
        .collect()
}

/// Checks oracle 1 and returns the re-opened output.
fn assert_packets_copied(src: &[u8], out: &[u8], plans: &[CutPlan]) -> OggFile {
    let f = open(src);
    let o = open(out);
    assert!(o.warnings().is_empty(), "{:?}", o.warnings());
    assert_eq!(o.codec(), f.codec());
    assert_eq!(o.index().info(), f.index().info());
    let want: Vec<_> = plans
        .iter()
        .flat_map(|p| payloads(&f, src, p.packets.clone()))
        .collect();
    let got = payloads(&o, out, 0..o.index().packets().len());
    assert_eq!(got.len(), want.len(), "packet count");
    assert!(got == want, "packet payloads differ");
    let len: u64 = plans.iter().map(CutPlan::output_len).sum();
    assert_eq!(o.index().total_samples(), len);
    assert_eq!(granule_length(out), len, "granule positions");
    o
}

/// Total length announced by the granule positions: per logical stream the
/// last granule minus the Opus pre-skip. Also checks that granules never
/// decrease within a stream.
fn granule_length(out: &[u8]) -> u64 {
    use lac_container::ogg::page::{FLAG_BOS, NO_GRANULE, parse_page};
    let mut total = 0u64;
    let (mut pos, mut last, mut pre_skip) = (0usize, 0i64, 0i64);
    while pos < out.len() {
        let h = parse_page(&out[pos..]).expect("valid page");
        let body = &out[pos + h.header_len()..];
        if h.flags & FLAG_BOS != 0 {
            total += (last - pre_skip) as u64;
            last = 0;
            // RFC 7845 5.1: OpusHead pre-skip at offset 10.
            pre_skip = if body.starts_with(b"OpusHead") {
                i64::from(u16::from_le_bytes([body[10], body[11]]))
            } else {
                0
            };
        }
        if h.granule != NO_GRANULE {
            assert!(h.granule >= last, "granule decreases at byte {pos}");
            last = h.granule;
        }
        pos += h.header_len() + h.body_len();
    }
    total + (last - pre_skip) as u64
}

/// Whether ffmpeg decodes the stream written for `plan` too long (see the
/// `ogg` module docs): an Opus range that starts and ends inside the same
/// packet. ffmpeg forgets the rest of the pre-skip when the end-trimmed
/// last packet arrives. The end trim is either the plan's own or the source
/// link's end trim, when the first packet is the last one of a link (of the
/// file, or of a link in a chained file: a plan crossing a link boundary
/// is written as one stream per link). The demuxer shortens such a packet,
/// so a plan ending there has `trim_end` 0; counting every such plan is
/// conservative. [`opus_sliver_inside_one_packet`] and
/// [`opus_sliver_in_the_end_trimmed_last_packet`] cover it.
fn ffmpeg_mistrims(f: &OggFile, src: &[u8], plan: &CutPlan) -> bool {
    let Some(first) = f.index().packets().get(plan.packets.start) else {
        return false;
    };
    let first_end = first.first_sample + u64::from(first.n_samples);
    let ends_link = link_ends(f, src).contains(&first_end);
    let own_trim = plan.packets.len() == 1 && plan.trim_end > 0;
    plan.trim_start > 0 && (ends_link || own_trim)
}

/// Timeline positions where the links of `f` end.
fn link_ends(f: &OggFile, src: &[u8]) -> Vec<u64> {
    let sections = f.decode_sections(&mut Cursor::new(src)).expect("sections");
    sections
        .iter()
        .scan(0, |end, s| {
            *end += s.keep.end - s.keep.start;
            Some(*end)
        })
        .collect()
}

/// Checks oracles 2 and 3: the independent decoder's output for `out` is
/// its output for `src` restricted to the plans' actual ranges.
///
/// Also valid for chained sources: oggdec and ffmpeg's Opus decoder start
/// every link with a fresh decoder and honour its end trim, and so does
/// our timeline, so a link boundary is no join point.
fn assert_decodes_like_source(name: &str, src: &[u8], out: &[u8], plans: &[CutPlan]) {
    let f = open(src);
    if f.codec() == CodecKind::Opus && plans.iter().any(|p| ffmpeg_mistrims(&f, src, p)) {
        return;
    }
    let info = f.index().info();
    let frame = sample_bytes(f.codec()) * usize::from(info.channels);
    let src_path = temp_file(&format!("src-{name}"), src);
    let out_path = temp_file(&format!("out-{name}"), out);
    let (Some(s), Some(o)) = (decode(f.codec(), &src_path), decode(f.codec(), &out_path)) else {
        return;
    };
    let total: u64 = plans.iter().map(CutPlan::output_len).sum();
    assert_eq!(o.len() / frame, total as usize, "{name}: decoded length");

    let window = match f.codec() {
        CodecKind::Vorbis => 0,
        CodecKind::Opus => (OPUS_JOIN_WINDOW * f64::from(info.sample_rate)) as usize,
        CodecKind::Speex => (SPEEX_JOIN_WINDOW * f64::from(info.sample_rate)) as usize,
    };
    // The source decode may lack the end padding; compare what exists.
    let src_len = s.len() / frame;
    let mut out_pos = 0usize;
    let mut prev_end = 0u64;
    // Speex keeps every part in one stream, so the decoder state of a
    // join carries into all later parts, even contiguous ones.
    let mut after_join = false;
    // Source position where the window of the latest join ends. A part
    // shorter than the window does not end it: the next part continues in
    // the same decoder state, so its window is what is left over.
    let mut window_end = 0usize;
    for p in plans {
        let r = p.actual.start as usize..p.actual.end as usize;
        // A range continuing where the previous one ended, or starting at
        // the very beginning, is no join point.
        if r.start as u64 != prev_end {
            after_join = true;
            window_end = r.start + window;
        }
        let skip = window_end.saturating_sub(r.start);
        let tolerance = if f.codec() == CodecKind::Speex && after_join {
            SPEEX_JOIN_TOLERANCE
        } else {
            0.0
        };
        let a = (r.start + skip).min(r.end).min(src_len);
        let b = r.end.min(src_len);
        if a < b {
            let want = &s[a * frame..b * frame];
            let got_at = out_pos + (a - r.start);
            let got = &o[got_at * frame..(got_at + b - a) * frame];
            if let Some(i) = (0..b - a).find(|&i| {
                !frames_match(
                    &want[i * frame..(i + 1) * frame],
                    &got[i * frame..(i + 1) * frame],
                    tolerance,
                )
            }) {
                panic!(
                    "{name}: range {r:?} differs at source sample {} ({} samples after the allowed window)",
                    a + i,
                    i
                );
            }
        }
        out_pos += r.len();
        prev_end = p.actual.end;
    }
    let _ = std::fs::remove_file(src_path);
    let _ = std::fs::remove_file(out_path);
}

/// Whether two decoded frames agree: bit for bit when `tolerance` is 0,
/// otherwise each f32 sample within `tolerance` (only used for Speex, whose
/// decode is f32le).
fn frames_match(want: &[u8], got: &[u8], tolerance: f32) -> bool {
    if tolerance == 0.0 {
        return want == got;
    }
    let f = |b: &[u8]| -> Vec<f32> {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|&x| f32::from_le_bytes(x))
            .collect()
    };
    f(want)
        .iter()
        .zip(f(got))
        .all(|(w, g)| (w - g).abs() <= tolerance)
}

/// Decodes with the independent decoder for `codec` (see oracle 2).
fn decode(codec: CodecKind, path: &std::path::Path) -> Option<Vec<u8>> {
    match codec {
        CodecKind::Vorbis => oggdec_decode(path),
        CodecKind::Opus | CodecKind::Speex => ffmpeg_decode(path, "f32le"),
    }
}

/// Bytes per sample of [`decode`]'s output.
fn sample_bytes(codec: CodecKind) -> usize {
    match codec {
        CodecKind::Vorbis => 2,
        CodecKind::Opus | CodecKind::Speex => 4,
    }
}

// ------------------------------------------------------------ demuxing

#[test]
fn fixtures_open_cleanly() {
    for (name, codec, rate, channels, len) in FIXTURES {
        let f = open(&fixture(name));
        assert!(f.warnings().is_empty(), "{name}: {:?}", f.warnings());
        assert_eq!(f.codec(), codec, "{name}");
        assert_eq!(f.links(), 1, "{name}");
        assert_eq!(f.index().info().sample_rate, rate, "{name}");
        assert_eq!(f.index().info().channels, channels, "{name}");
        assert_eq!(f.index().total_samples(), len, "{name}");
    }
}

/// Our packet-derived length agrees with an independent decoder: exactly
/// for Vorbis and Opus, whose end trim we honour, and for Speex up to the end padding
/// that the decoder trims according to the last granule position.
#[test]
fn length_agrees_with_ffmpeg() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let path = temp_file(name, &src);
        let Some(d) = decode(f.codec(), &path) else {
            return;
        };
        let decoded =
            (d.len() / (sample_bytes(f.codec()) * usize::from(f.index().info().channels))) as u64;
        let ours = f.index().total_samples();
        let last = u64::from(f.index().packets().last().map_or(0, |p| p.n_samples));
        if f.codec() != CodecKind::Speex {
            assert_eq!(decoded, ours, "{name}");
        } else {
            assert!(
                decoded <= ours && ours - decoded < last,
                "{name}: decoder {decoded}, ours {ours}"
            );
        }
    }
}

/// A chained file is the concatenation of its links, each with its own
/// end trim applied: the encoder padding at the end of the first link is
/// not part of the timeline, exactly as in libvorbisfile, opusfile and
/// ffmpeg. Before the Vorbis end trim was honoured, 142 samples of padding
/// were played at the link boundary (and at the end).
#[test]
fn chained_files_are_one_timeline() {
    for (name, codec, rate) in [
        ("ogg_vorbis_chained.ogg", CodecKind::Vorbis, 44_100),
        ("ogg_opus_chained.opus", CodecKind::Opus, 48_000),
    ] {
        let src = fixture(name);
        let f = open(&src);
        assert!(f.warnings().is_empty(), "{name}: {:?}", f.warnings());
        assert_eq!(f.codec(), codec);
        assert_eq!(f.links(), 2, "{name}");
        // 0.5 s + 0.8 s of input (see generate.sh), nothing more.
        assert_eq!(f.index().total_samples(), rate * 13 / 10, "{name}");
        // Each link is offered as a track; the second starts after the
        // first one's 0.5 s (its end trim applied).
        let starts: Vec<u64> = f.chapters().iter().map(|c| c.start).collect();
        assert_eq!(starts, [0, rate / 2], "{name}");
        // The reference decoder agrees on the length.
        let path = temp_file(name, &src);
        if let Some(d) = decode(codec, &path) {
            let frame = sample_bytes(codec) * usize::from(f.index().info().channels);
            assert_eq!((d.len() / frame) as u64, rate * 13 / 10, "{name}");
        }
    }
}

/// Random granule positions, as some radio stations send them, change
/// nothing: the timeline and the cut output are the same as for the clean
/// file. The only difference is the clean file's end trim, which is
/// not trusted when the granules before it are garbage, so the damaged
/// file's last packet is longer.
#[test]
fn granule_positions_are_ignored() {
    for (clean, bad) in [
        ("ogg_vorbis_stereo_44k.ogg", "ogg_vorbis_badgranule.ogg"),
        ("ogg_opus_stereo.opus", "ogg_opus_badgranule.opus"),
    ] {
        let (c, b) = (fixture(clean), fixture(bad));
        assert_ne!(c, b);
        let (fc, fb) = (open(&c), open(&b));
        assert!(fb.warnings().is_empty(), "{bad}: {:?}", fb.warnings());
        let (pc, pb) = (fc.index().packets(), fb.index().packets());
        let n = pc.len();
        assert_eq!(pb.len(), n, "{bad}");
        assert_eq!(pc[..n - 1], pb[..n - 1], "{bad}");
        let trimmed = pb[n - 1].n_samples - pc[n - 1].n_samples;
        match fc.codec() {
            // ffmpeg's libopus wrapper pads the end to a whole packet.
            CodecKind::Opus => assert_eq!(trimmed, 648, "{bad}"),
            // libvorbis pads the last block: 66_176 decoded, 66_150 input.
            CodecKind::Vorbis => assert_eq!(trimmed, 26, "{bad}"),
            _ => assert_eq!(trimmed, 0, "{bad}"),
        }
        let len = pc[n - 1].first_sample;
        let p = plans(&fc, &[len / 5..len / 2, len * 3 / 4..len]);
        assert_eq!(write(&c, &fc, &p), write(&b, &fb, &p), "{bad}");
    }
}

#[test]
fn damaged_page_is_skipped_with_warning() {
    let clean = open(&fixture("ogg_vorbis_stereo_44k.ogg"));
    let src = fixture("ogg_vorbis_damaged.ogg");
    let f = open(&src);
    assert_eq!(f.warnings().len(), 1, "{:?}", f.warnings());
    assert!(f.warnings()[0].contains("damaged"), "{:?}", f.warnings());
    assert!(f.index().total_samples() < clean.index().total_samples());
    // The rest is still cuttable into a clean file.
    let p = plans(&f, &[0..f.index().total_samples()]);
    let out = write(&src, &f, &p);
    assert_packets_copied(&src, &out, &p);
}

#[test]
fn garbage_is_rejected_without_panic() {
    for data in [
        b"OggS".to_vec(),
        common::noise(10_000, 7),
        [b"OggS".as_slice(), &common::noise(5_000, 3)].concat(),
    ] {
        assert!(OggFile::open(&mut Cursor::new(&data)).is_err());
    }
}

#[test]
fn truncated_files_open() {
    let src = fixture("ogg_opus_stereo.opus");
    let full = open(&src).index().total_samples();
    for cut in [src.len() / 3, src.len() / 2, src.len() - 1] {
        let f = open(&src[..cut]);
        assert!(f.index().total_samples() < full);
        assert!(
            f.warnings().iter().any(|w| w.contains("truncated")),
            "{:?}",
            f.warnings()
        );
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
        let o = assert_packets_copied(&src, &out, &p);
        assert_eq!(o.links(), 1, "{name}");
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

#[test]
fn keep_middle_part() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let p = plans(&f, &[len / 5..len * 4 / 5]);
        let out = write(&src, &f, &p);
        assert_packets_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

#[test]
fn deletes_become_chained_streams() {
    for (name, ..) in FIXTURES {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let p = delete(&f, &[(len / 6, len / 3), (len / 2, len * 2 / 3)]);
        let out = write(&src, &f, &p);
        let o = assert_packets_copied(&src, &out, &p);
        // Speex joins within one stream (see the `ogg` module docs).
        let streams = if f.codec() == CodecKind::Speex { 1 } else { 3 };
        assert_eq!(o.links(), streams, "{name}");
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// Regression (found by `random_deletes_copy_packets`): a kept part of a
/// single Vorbis packet becomes a two-packet chained stream (priming packet
/// plus that one). With both on one page, which is then the first and the
/// last audio page, libvorbisfile dropped the whole stream from the chain.
#[test]
fn one_packet_vorbis_part_survives_chaining() {
    let name = "ogg_vorbis_stereo_44k.ogg";
    let src = fixture(name);
    let f = open(&src);
    let p = delete(&f, &[(39, 1969)]);
    assert_eq!(p[0].packets.len(), 1, "the first part is a single packet");
    let out = write(&src, &f, &p);
    assert_packets_copied(&src, &out, &p);
    assert_decodes_like_source(name, &src, &out, &p);
}

/// Opus: a cut inside the source's pre-skip keeps exactly the audible part.
#[test]
fn opus_cut_near_start_respects_pre_skip() {
    let name = "ogg_opus_stereo.opus";
    let src = fixture(name);
    let f = open(&src);
    for start in [0, 1, 100, 960] {
        let p = plans(&f, &[start..20_000]);
        let out = write(&src, &f, &p);
        assert_packets_copied(&src, &out, &p);
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// Opus cuts are sample-exact at both ends of every kept range, including
/// ranges that start and end in the middle of a packet, ranges within one
/// packet, and cuts at the end of a chained source's links.
#[test]
fn opus_cuts_are_sample_exact() {
    let name = "ogg_opus_stereo.opus";
    let src = fixture(name);
    let f = open(&src);
    assert!(f.capabilities().can_trim_start && f.capabilities().can_trim_end);
    let len = f.index().total_samples();
    for ranges in [
        vec![12_345..54_321],
        vec![10_100..10_900],
        vec![0..len - 1],
        vec![7..20_007, 20_013..40_000, 50_001..len - 3],
    ] {
        let p = plans(&f, &ranges);
        for (plan, r) in p.iter().zip(&ranges) {
            assert_eq!(plan.actual, *r, "{ranges:?}");
        }
        let out = write(&src, &f, &p);
        let o = assert_packets_copied(&src, &out, &p);
        assert_eq!(o.links(), ranges.len(), "{ranges:?}");
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// A kept range inside a single Opus packet is written correctly (RFC 7845:
/// pre-skip and end trim inside one packet; our demuxer and granule
/// positions agree on its length). ffmpeg plays it too long, by the part of
/// the pre-skip that lies inside the packet; this test documents that, and
/// fails once ffmpeg handles it, so [`ffmpeg_mistrims`] can go.
#[test]
fn opus_sliver_inside_one_packet() {
    let name = "ogg_opus_stereo.opus";
    let src = fixture(name);
    let f = open(&src);
    // Packet 1 holds source samples 648..1608.
    let p = plans(&f, &[1_000..1_001]);
    assert!(ffmpeg_mistrims(&f, &src, &p[0]));
    let out = write(&src, &f, &p);
    assert_packets_copied(&src, &out, &p);
    let path = temp_file(&format!("sliver-{name}"), &out);
    if let Some(d) = decode(CodecKind::Opus, &path) {
        let decoded = d.len() / (sample_bytes(CodecKind::Opus) * 2);
        // FFmpeg 8 decodes the planned single sample; older versions play
        // the packet from its start (the documented mistrim).
        assert!(
            decoded == 1 || decoded == 1 + (1_000 - 648),
            "{name}: {decoded} samples"
        );
    }
}

/// Found by `random_deletes_copy_packets`: keeping only the tail of the
/// source's last packet, which the source itself end-trims (RFC 7845 4.4),
/// is the same ffmpeg mistrim as [`opus_sliver_inside_one_packet`], although
/// the plan has no end trim of its own. Our output has the planned length.
#[test]
fn opus_sliver_in_the_end_trimmed_last_packet() {
    let name = "ogg_opus_stereo.opus";
    let src = fixture(name);
    let f = open(&src);
    let len = f.index().total_samples();
    // The fixture's last packet holds source samples 71_688..72_000, the
    // rest of it is the encoder's end trim.
    let p = plans(&f, &[0..67_497, 71_771..len]);
    assert_eq!(p[1].packets.len(), 1);
    assert_eq!(p[1].trim_end, 0);
    assert!(ffmpeg_mistrims(&f, &src, &p[1]));
    let out = write(&src, &f, &p);
    let o = assert_packets_copied(&src, &out, &p);
    assert_eq!(o.index().total_samples(), 67_497 + (len - 71_771));
}

/// A delete inside a single Opus packet keeps that packet twice: once to
/// end the stream before the delete, once to start the one after it.
#[test]
fn opus_delete_inside_one_packet() {
    let name = "ogg_opus_stereo.opus";
    let src = fixture(name);
    let f = open(&src);
    let p = delete(&f, &[(30_100, 30_400)]);
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].packets.end - 1, p[1].packets.start);
    let out = write(&src, &f, &p);
    let o = assert_packets_copied(&src, &out, &p);
    assert_eq!(o.links(), 2);
    assert_eq!(o.index().total_samples(), f.index().total_samples() - 300);
    assert_decodes_like_source(name, &src, &out, &p);
}

/// Our own end trims survive a round trip: re-opening a cut and keeping all
/// of it reproduces it byte for byte, and its index has the cut's length.
#[test]
fn opus_cut_of_cut_is_stable() {
    let src = fixture("ogg_opus_chained.opus");
    let f = open(&src);
    let len = f.index().total_samples();
    let p = delete(&f, &[(len / 3 + 17, len / 2 + 5)]);
    let once = write(&src, &f, &p);
    let o = open(&once);
    assert_eq!(
        o.index().total_samples(),
        len - (len / 2 + 5 - len / 3 - 17)
    );
    let again = write(&once, &o, &plans(&o, &[0..o.index().total_samples()]));
    assert_eq!(once, again);
}

#[test]
fn cut_across_chain_boundary() {
    for name in ["ogg_vorbis_chained.ogg", "ogg_opus_chained.opus"] {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        let p = plans(&f, &[len / 4..len * 3 / 4]);
        let out = write(&src, &f, &p);
        let o = assert_packets_copied(&src, &out, &p);
        assert_eq!(o.links(), 2, "{name}");
        assert_decodes_like_source(name, &src, &out, &p);
    }
}

/// Cuts that end exactly at a link boundary, start exactly there, delete
/// the boundary or keep only the end-trimmed last packet of the first link:
/// the output keeps the source link's end trim wherever it keeps the
/// trimmed packet, so it decodes exactly like the source.
#[test]
fn cuts_at_chain_boundary() {
    for name in ["ogg_vorbis_chained.ogg", "ogg_opus_chained.opus"] {
        let src = fixture(name);
        let f = open(&src);
        let len = f.index().total_samples();
        // The first link holds 0.5 s.
        let b = u64::from(f.index().info().sample_rate) / 2;
        for ranges in [
            vec![0..b],
            vec![b..len],
            vec![b / 2..b, b..len / 2 + b / 2],
            vec![0..b / 2, b + b / 4..len],
            vec![b - 1..b + 1],
        ] {
            let p = plans(&f, &ranges);
            let out = write(&src, &f, &p);
            assert_packets_copied(&src, &out, &p);
            assert_decodes_like_source(name, &src, &out, &p);
            // Cutting the cut again changes nothing.
            let o = open(&out);
            let again = write(&out, &o, &plans(&o, &[0..o.index().total_samples()]));
            assert_eq!(out, again, "{name} {ranges:?}");
        }
    }
}

#[test]
fn cut_of_cut_is_stable() {
    let name = "ogg_vorbis_stereo_44k.ogg";
    let src = fixture(name);
    let f = open(&src);
    let len = f.index().total_samples();
    let p = delete(&f, &[(len / 3, len / 2)]);
    let once = write(&src, &f, &p);
    let o = open(&once);
    // Keeping everything of a chained output keeps it as it is.
    let again = write(&once, &o, &plans(&o, &[0..o.index().total_samples()]));
    assert_eq!(once, again);
}

/// Regression: a join inside a pause whose decoder state survives the next
/// pause; it took 267 ms to become bit-identical, more than the 250 ms the
/// tests used to allow (found by `random_deletes_copy_packets`).
#[test]
fn speex_join_converges_within_window() {
    let name = "ogg_speex_16k.spx";
    let src = fixture(name);
    let f = open(&src);
    let p = delete(&f, &[(1280, 3520)]);
    let out = write(&src, &f, &p);
    assert_decodes_like_source(name, &src, &out, &p);
}

#[test]
fn empty_cut_writes_nothing() {
    let src = fixture("ogg_speex_16k.spx");
    let f = open(&src);
    assert!(write(&src, &f, &plans(&f, &[100..100])).is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Random delete sequences on every fixture, including chained ones:
    /// packets are copied verbatim, the output is a valid Ogg file with the
    /// planned length, and (where ffmpeg is installed) decodes like the
    /// source outside the documented join windows.
    #[test]
    fn random_deletes_copy_packets(
        which in 0usize..FIXTURES.len() + 2,
        deletes in proptest::collection::vec((0.0f64..1.0, 0.0f64..0.3), 1..4),
    ) {
        let name = match which {
            i if i < FIXTURES.len() => FIXTURES[i].0,
            i if i == FIXTURES.len() => "ogg_vorbis_chained.ogg",
            _ => "ogg_opus_chained.opus",
        };
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

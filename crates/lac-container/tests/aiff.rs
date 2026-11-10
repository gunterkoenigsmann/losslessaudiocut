//! Tests of the AIFF / AIFF-C demuxer and muxer.
//!
//! Most files are built in memory by [`build_aiff`] from deterministic
//! pseudo-random samples; the committed fixtures (written by ffmpeg, see
//! `tests/fixtures/generate.sh`) cover what another writer produces:
//! big-endian 16 and 24 bit, AIFF-C `sowt` and `fl32`, and tags.
//!
//! Oracles: for PCM the stored bytes *are* the samples, so a cut must
//! hold exactly the source's frame bytes minus the deleted ones; and
//! ffmpeg, an independent demuxer and decoder, must decode the cut like
//! the source with the same frames removed.

mod common;

use std::io::Cursor;

use lac_container::ContainerError;
use lac_container::aiff::AiffFile;
use lac_container::wav::{BLOCK_FRAMES, SampleEncoding};
use lac_core::{EditList, SourceId, Tags, plan_cut};
use proptest::prelude::*;

use common::{ffmpeg_decode, fixture, noise, temp_file};

/// 80-bit extended float of a whole sample rate (AIFF 1.3 COMM).
fn rate_bytes(rate: u32) -> [u8; 10] {
    let lz = rate.leading_zeros();
    let exp = 16383 + 31 - lz as u16;
    let mantissa = u64::from(rate) << (32 + lz);
    let mut b = [0u8; 10];
    b[..2].copy_from_slice(&exp.to_be_bytes());
    b[2..].copy_from_slice(&mantissa.to_be_bytes());
    b
}

fn chunk(id: &[u8; 4], payload: &[u8], declared: Option<u32>) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend(declared.unwrap_or(payload.len() as u32).to_be_bytes());
    c.extend(payload);
    if payload.len() % 2 == 1 {
        c.push(0);
    }
    c
}

/// Parameters of a generated file; `compression` set makes it AIFF-C.
#[derive(Clone, Copy)]
struct Spec {
    channels: u16,
    rate: u32,
    bits: u16,
    compression: Option<&'static [u8; 4]>,
}

const S16_STEREO: Spec = Spec {
    channels: 2,
    rate: 44_100,
    bits: 16,
    compression: None,
};

/// Builds an AIFF(-C) file. `frames` overrides COMM's frame count,
/// `ssnd_offset` puts that many filler bytes in front of the samples, and
/// `extra` chunks go between COMM and SSND.
fn build_aiff(
    s: Spec,
    data: &[u8],
    frames: Option<u32>,
    ssnd_offset: u32,
    extra: &[Vec<u8>],
) -> Vec<u8> {
    let bytes = match s.compression {
        Some(b"fl32") => 4,
        Some(b"fl64") => 8,
        _ => s.bits.div_ceil(8),
    };
    let ba = usize::from(s.channels * bytes);
    let mut comm = s.channels.to_be_bytes().to_vec();
    comm.extend(frames.unwrap_or((data.len() / ba) as u32).to_be_bytes());
    comm.extend(s.bits.to_be_bytes());
    comm.extend(rate_bytes(s.rate));
    let mut body = Vec::new();
    if let Some(c) = s.compression {
        comm.extend(c);
        comm.extend([3, b'a', b'b', b'c']); // pascal string, even length
        body.extend(b"AIFC");
        body.extend(chunk(b"FVER", &0xA280_5140u32.to_be_bytes(), None));
    } else {
        body.extend(b"AIFF");
    }
    body.extend(chunk(b"COMM", &comm, None));
    for c in extra {
        body.extend(c);
    }
    let mut ssnd = ssnd_offset.to_be_bytes().to_vec();
    ssnd.extend(0u32.to_be_bytes());
    ssnd.extend(vec![0xEE; ssnd_offset as usize]);
    ssnd.extend(data);
    body.extend(chunk(b"SSND", &ssnd, None));
    let mut f = b"FORM".to_vec();
    f.extend((body.len() as u32).to_be_bytes());
    f.extend(body);
    f
}

fn open(bytes: &[u8]) -> Result<AiffFile, ContainerError> {
    AiffFile::open(&mut Cursor::new(bytes))
}

// ---------------------------------------------------------------- parsing

#[test]
fn parses_basic_aiff() {
    let data = noise(4 * 10_000, 1);
    let a = open(&build_aiff(S16_STEREO, &data, None, 0, &[])).expect("valid");
    assert_eq!(a.info().sample_rate, 44_100);
    assert_eq!(a.info().channels, 2);
    assert_eq!(a.encoding(), SampleEncoding::Pcm);
    assert!(a.big_endian() && !a.is_aifc());
    assert_eq!(a.block_align(), 4);
    assert_eq!(a.index().total_samples(), 10_000);
    assert_eq!(
        a.index().packets().len(),
        10_000usize.div_ceil(BLOCK_FRAMES as usize)
    );
    assert!(a.warnings().is_empty(), "{:?}", a.warnings());
}

#[test]
fn ssnd_offset_is_skipped() {
    let data = noise(4 * 100, 2);
    let f = build_aiff(S16_STEREO, &data, None, 6, &[]);
    let a = open(&f).expect("valid");
    assert_eq!(a.index().total_samples(), 100);
    let got = a.read_frames(&mut Cursor::new(&f), 0..100).expect("read");
    assert_eq!(got, data);
}

#[test]
fn wrong_frame_counts_use_the_audio_present() {
    let data = noise(4 * 100, 3);
    // A crashed recorder: no frames claimed.
    let a = open(&build_aiff(S16_STEREO, &data, Some(0), 0, &[])).expect("valid");
    assert_eq!(a.index().total_samples(), 100);
    assert_eq!(a.warnings().len(), 1, "{:?}", a.warnings());
    // More claimed than present.
    let a = open(&build_aiff(S16_STEREO, &data, Some(500), 0, &[])).expect("valid");
    assert_eq!(a.index().total_samples(), 100);
    assert_eq!(a.warnings().len(), 1, "{:?}", a.warnings());
    // Fewer claimed: COMM is authoritative, the rest is padding.
    let a = open(&build_aiff(S16_STEREO, &data, Some(60), 0, &[])).expect("valid");
    assert_eq!(a.index().total_samples(), 60);
    assert!(a.warnings().is_empty(), "{:?}", a.warnings());
}

#[test]
fn placeholder_ssnd_size_uses_the_rest_of_the_file() {
    let data = noise(4 * 100, 4);
    let mut f = build_aiff(S16_STEREO, &data, None, 0, &[]);
    let ssnd = f.len() - data.len() - 16;
    assert_eq!(&f[ssnd..ssnd + 4], b"SSND");
    f[ssnd + 4..ssnd + 8].copy_from_slice(&0u32.to_be_bytes());
    let a = open(&f).expect("valid");
    assert_eq!(a.index().total_samples(), 100);
    assert_eq!(a.warnings().len(), 1, "{:?}", a.warnings());
}

#[test]
fn rejects_what_it_cannot_cut() {
    assert!(matches!(
        open(b"FORM\0\0\0\x04WAVE"),
        Err(ContainerError::Malformed { .. })
    ));
    let ima = Spec {
        compression: Some(b"ima4"),
        ..S16_STEREO
    };
    assert!(matches!(
        open(&build_aiff(ima, &noise(400, 5), None, 0, &[])),
        Err(ContainerError::Unsupported(_))
    ));
    // No COMM chunk.
    let mut f = b"FORM\0\0\0\x14AIFF".to_vec();
    f.extend(chunk(b"SSND", &[0; 12], None));
    assert!(matches!(open(&f), Err(ContainerError::Malformed { .. })));
}

#[test]
fn text_chunks_and_id3_are_read() {
    let extra = [
        chunk(b"NAME", b"Name", None),
        chunk(b"AUTH", b"Author", None),
        chunk(b"ANNO", b"first", None),
        chunk(b"ANNO", b"second", None),
        chunk(b"(c) ", b"2026 lac", None),
    ];
    let a = open(&build_aiff(S16_STEREO, &noise(400, 6), None, 0, &extra)).expect("valid");
    let t = a.tags();
    assert_eq!(t.title.as_deref(), Some("Name"));
    assert_eq!(t.artist.as_deref(), Some("Author"));
    assert_eq!(t.comment.as_deref(), Some("first\nsecond"));
    assert_eq!(t.extra, vec![("COPYRIGHT".into(), "2026 lac".into())]);

    let f = open(&fixture("aiff_s16_44k.aiff")).expect("fixture");
    assert_eq!(f.tags().title.as_deref(), Some("Fixture"));
    assert_eq!(f.tags().artist.as_deref(), Some("losslessaudiocut"));
}

// ---------------------------------------------------------------- cutting

/// Cuts `src` with the given delete ranges (in timeline coordinates,
/// applied in order) and returns the output file.
fn cut(src: &[u8], deletes: &[(u64, u64)], tags: Option<&Tags>) -> Vec<u8> {
    let a = open(src).expect("valid source");
    let mut edit = EditList::whole(SourceId(0), a.index().total_samples(), Tags::default());
    for &(x, y) in deletes {
        edit.tracks[0].delete(x..y).expect("valid delete");
    }
    let plans: Vec<_> = edit.tracks[0]
        .segments
        .iter()
        .map(|s| plan_cut(a.index(), a.capabilities(), s.range.clone()).expect("plan"))
        .collect();
    let mut out = Vec::new();
    a.write_cut(&mut Cursor::new(src), &plans, tags, &mut out)
        .expect("write");
    out
}

#[test]
fn delete_middle_and_join() {
    let data = noise(4 * 10_000, 9);
    let src = build_aiff(S16_STEREO, &data, None, 0, &[]);
    let out = cut(&src, &[(1_000, 6_000)], None);
    let o = open(&out).expect("output is valid AIFF");
    assert!(o.warnings().is_empty(), "{:?}", o.warnings());
    assert_eq!(o.index().total_samples(), 5_000);
    let got = o
        .read_frames(&mut Cursor::new(&out), 0..5_000)
        .expect("read");
    let mut want = data[..4_000].to_vec();
    want.extend(&data[24_000..]);
    assert_eq!(got, want);
    // The FORM size covers the whole file.
    let form = u32::from_be_bytes([out[4], out[5], out[6], out[7]]) as usize;
    assert_eq!(form + 8, out.len());
}

#[test]
fn aifc_keeps_its_type_and_odd_output_is_padded() {
    // 8-bit mono A-law: 101 frames give an odd SSND chunk.
    let s = Spec {
        channels: 1,
        rate: 8_000,
        bits: 8,
        compression: Some(b"alaw"),
    };
    let src = build_aiff(s, &noise(200, 11), None, 0, &[]);
    let out = cut(&src, &[(0, 99)], None);
    assert_eq!(out.len() % 2, 0);
    let o = open(&out).expect("valid");
    assert!(o.is_aifc());
    assert_eq!(o.compression(), *b"alaw");
    assert_eq!(o.encoding(), SampleEncoding::ALaw);
    assert_eq!(o.index().total_samples(), 101);
    assert!(o.warnings().is_empty(), "{:?}", o.warnings());
}

#[test]
fn tags_survive_and_can_be_replaced() {
    let extra = [
        chunk(b"NAME", b"Name", None),
        chunk(b"(c) ", b"2026 lac", None),
    ];
    let src = build_aiff(S16_STEREO, &noise(4000, 12), None, 0, &extra);
    let kept = open(&cut(&src, &[(10, 20)], None)).expect("valid");
    assert_eq!(kept.tags(), open(&src).expect("valid").tags());
    let new = Tags {
        title: Some("Neu".into()),
        album: Some("Album".into()),
        track_number: Some(3),
        comment: Some("a\nb".into()),
        ..Tags::default()
    };
    let replaced = open(&cut(&src, &[(10, 20)], Some(&new))).expect("valid");
    assert_eq!(replaced.tags(), &new);
}

#[test]
fn delete_everything_gives_valid_empty_file() {
    let src = build_aiff(S16_STEREO, &noise(400, 12), None, 0, &[]);
    let o = open(&cut(&src, &[(0, 100)], None)).expect("valid");
    assert_eq!(o.index().total_samples(), 0);
}

proptest! {
    /// Golden rule: the output's samples equal the source's samples with
    /// the deleted ranges removed, bit for bit.
    #[test]
    fn cut_matches_reference(
        frames in 0u64..20_000,
        channels in 1u16..5,
        bits in 1u16..33,
        aifc in any::<bool>(),
        deletes in proptest::collection::vec((0u64..20_000, 0u64..8_000), 0..5),
        seed in any::<u64>(),
    ) {
        let s = Spec {
            channels,
            rate: 48_000,
            bits,
            compression: aifc.then_some(b"sowt"),
        };
        let ba = u64::from(channels * bits.div_ceil(8));
        let data = noise((frames * ba) as usize, seed);
        let src = build_aiff(s, &data, None, 0, &[]);

        let mut model: Vec<&[u8]> = data.chunks(ba as usize).collect();
        let mut dels = Vec::new();
        for (a, n) in deletes {
            let len = model.len() as u64;
            let a = a.min(len);
            let b = (a + n).min(len);
            model.drain(a as usize..b as usize);
            dels.push((a, b));
        }
        let out = cut(&src, &dels, None);
        let o = open(&out).expect("output is valid AIFF");
        prop_assert_eq!(o.index().total_samples(), model.len() as u64);
        prop_assert_eq!(o.sample_size(), bits);
        let got = o.read_frames(&mut Cursor::new(&out), 0..model.len() as u64).expect("read");
        prop_assert_eq!(got, model.concat());
    }
}

// ------------------------------------------------- independent cross-check

/// The ffmpeg fixtures: (name, AIFF-C, channels, frames).
const FIXTURES: [(&str, bool, u16, u64); 4] = [
    ("aiff_s16_44k.aiff", false, 2, 22_050),
    ("aiff_s24_48k_mono.aif", false, 1, 19_200),
    ("aifc_sowt_22k.aifc", true, 2, 8_820),
    ("aifc_fl32_22k.aifc", true, 2, 8_820),
];

#[test]
fn ffmpeg_fixtures_open() {
    for (name, aifc, channels, frames) in FIXTURES {
        let a = open(&fixture(name)).expect(name);
        assert!(a.warnings().is_empty(), "{name}: {:?}", a.warnings());
        assert_eq!(a.is_aifc(), aifc, "{name}");
        assert_eq!(a.info().channels, channels, "{name}");
        assert_eq!(a.index().total_samples(), frames, "{name}");
    }
}

/// ffmpeg decodes our cut of each fixture like the source with the same
/// frames removed, bit for bit (as 32-bit float, which holds every
/// sample of these formats exactly).
#[test]
fn ffmpeg_agrees_with_our_output() {
    for (name, _, channels, frames) in FIXTURES {
        let src = fixture(name);
        let deletes = [(100, 3_000), (frames / 2, frames / 2 + 777)];
        let out = cut(&src, &deletes, None);
        let ext = name.rsplit('.').next().unwrap_or("aiff");
        let (Some(s), Some(o)) = (
            ffmpeg_decode(&temp_file(&format!("src.{ext}"), &src), "f32le"),
            ffmpeg_decode(&temp_file(&format!("out.{ext}"), &out), "f32le"),
        ) else {
            return;
        };
        let fb = 4 * usize::from(channels);
        let mut model: Vec<&[u8]> = s.chunks(fb).collect();
        for (a, b) in deletes {
            model.drain(a as usize..b as usize);
        }
        assert_eq!(o.len(), model.len() * fb, "{name}: length");
        assert!(
            o == model.concat(),
            "{name}: ffmpeg decodes different samples"
        );
    }
}

/// Tracks in another order than the source's (issue #85): ranges written
/// out of order join like deletes, with the same window at each join.
#[test]
fn ranges_out_of_order_keep_their_samples() {
    let data = noise(4 * 10_000, 12);
    let src = build_aiff(S16_STEREO, &data, None, 0, &[]);
    let a = open(&src).expect("valid source");
    let plans: Vec<_> = [7_000..9_000, 1_000..3_000]
        .into_iter()
        .map(|r| plan_cut(a.index(), a.capabilities(), r).expect("plan"))
        .collect();
    let mut out = Vec::new();
    a.write_cut(&mut Cursor::new(&src), &plans, None, &mut out)
        .expect("write");
    let o = open(&out).expect("output is valid AIFF");
    let got = o
        .read_frames(&mut Cursor::new(&out), 0..4_000)
        .expect("read");
    let mut want = data[28_000..36_000].to_vec();
    want.extend(&data[4_000..12_000]);
    assert_eq!(got, want);
}

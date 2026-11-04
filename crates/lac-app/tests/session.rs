//! Tests of the open/export use case on generated WAV files.
//!
//! The source is a ramp (sample *n* has the value *n*), so the exported data
//! shows exactly which samples were kept: the golden rule
//! `decode(cut(file, range)) == decode(file)[range]` becomes a comparison of
//! integer lists.

// A list holding a single sample range is exactly what these tests mean.
#![allow(clippy::single_range_in_vec_init)]

use std::path::PathBuf;

use lac_app::{AppError, CutRequest, Source};

/// A fresh temporary directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lac-app-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// 16-bit mono PCM WAV at 1 kHz whose sample *n* has the value *n*.
fn ramp_wav(frames: u16) -> Vec<u8> {
    let data: Vec<u8> = (0..frames).flat_map(|n| n.to_le_bytes()).collect();
    let mut f = b"RIFF".to_vec();
    f.extend((36 + data.len() as u32).to_le_bytes());
    f.extend(b"WAVEfmt ");
    f.extend(16u32.to_le_bytes());
    f.extend(1u16.to_le_bytes()); // PCM
    f.extend(1u16.to_le_bytes()); // mono
    f.extend(1000u32.to_le_bytes()); // 1 kHz
    f.extend(2000u32.to_le_bytes()); // byte rate
    f.extend(2u16.to_le_bytes()); // block align
    f.extend(16u16.to_le_bytes()); // bits
    f.extend(b"data");
    f.extend((data.len() as u32).to_le_bytes());
    f.extend(data);
    f
}

/// The sample values of a file produced from `ramp_wav`, read with the
/// same plain 44-byte header layout (our muxer writes it that way for PCM).
fn samples(wav: &[u8]) -> Vec<u16> {
    wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect()
}

/// Writes a ramp of `frames` samples into `dir` and opens it.
fn ramp_source(dir: &std::path::Path, frames: u16) -> Source {
    let input = dir.join("in.wav");
    std::fs::write(&input, ramp_wav(frames)).expect("write input");
    Source::open(&input).expect("open input")
}

#[test]
fn open_reports_stream_properties() {
    let dir = temp_dir("open");
    let src = ramp_source(&dir, 1500);
    assert_eq!(src.format_name(), "WAV");
    assert_eq!(src.info().sample_rate, 1000);
    assert_eq!(src.info().channels, 1);
    assert_eq!(src.total_samples(), 1500);
    assert!(src.warnings().is_empty());
}

#[test]
fn delete_joins_the_rest_bit_exactly() {
    let dir = temp_dir("delete");
    let mut src = ramp_source(&dir, 1000);
    let out = dir.join("out.wav");
    // Unordered and overlapping on purpose: both refer to the original file.
    let req = CutRequest::Delete(vec![600..700, 100..200, 150..250]);
    let kept = src.export(&req, None, &out).expect("export");

    assert_eq!(
        kept.iter().map(|k| k.actual.clone()).collect::<Vec<_>>(),
        vec![0..100, 250..600, 700..1000]
    );
    assert!(kept.iter().all(|k| k.is_exact()), "PCM cuts are exact");
    let want: Vec<u16> = (0..100).chain(250..600).chain(700..1000).collect();
    assert_eq!(samples(&std::fs::read(&out).expect("read")), want);
}

#[test]
fn keep_preserves_the_given_order() {
    let dir = temp_dir("keep");
    let mut src = ramp_source(&dir, 1000);
    let out = dir.join("out.wav");
    let req = CutRequest::Keep(vec![800..900, 5..5, 10..20]);
    let kept = src.export(&req, None, &out).expect("export");

    assert_eq!(kept.len(), 2, "the empty range is dropped");
    let want: Vec<u16> = (800..900).chain(10..20).collect();
    assert_eq!(samples(&std::fs::read(&out).expect("read")), want);
}

#[test]
fn out_of_range_requests_are_errors() {
    let dir = temp_dir("range");
    let src = ramp_source(&dir, 100);
    for req in [
        CutRequest::Keep(vec![50..101]),
        #[allow(clippy::reversed_empty_ranges)] // reversed on purpose
        CutRequest::Keep(vec![60..50]),
        CutRequest::Delete(vec![90..200]),
    ] {
        assert!(
            matches!(src.kept_ranges(&req), Err(AppError::Edit(_))),
            "{req:?} should be rejected"
        );
    }
}

#[test]
fn refuses_to_overwrite_the_source() {
    let dir = temp_dir("same");
    let mut src = ramp_source(&dir, 100);
    let input = src.path().to_path_buf();
    let err = src
        .export(&CutRequest::Keep(vec![0..10]), None, &input)
        .expect_err("must refuse");
    assert!(matches!(err, AppError::OutputIsInput));
    assert_eq!(std::fs::read(&input).expect("read"), ramp_wav(100));
}

#[test]
fn missing_and_unsupported_files_are_errors() {
    let dir = temp_dir("bad");
    assert!(matches!(
        Source::open(dir.join("nope.wav")),
        Err(AppError::Open { .. })
    ));
    let junk = dir.join("junk.bin");
    std::fs::write(&junk, b"definitely not audio").expect("write");
    assert!(matches!(Source::open(&junk), Err(AppError::Read { .. })));
}

/// The waveform summary of a real recording matches its decoded samples:
/// per packet, min and max equal those of the decoded PCM.
#[test]
fn peaks_of_a_fixture_match_its_samples() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lac-container/tests/fixtures/mp3_cbr128_44k.mp3");
    let mut src = Source::open(&path).expect("open");
    let mut calls = 0;
    let (peaks, report) = src
        .build_peaks(|_| {
            calls += 1;
            lac_decode::Flow::Continue
        })
        .expect("decode");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(calls > 1, "progress is reported while decoding");
    assert_eq!(peaks.packets_done(), src.index().packets().len());

    // Independent decode of the same file.
    let mut r = std::io::BufReader::new(std::fs::File::open(&path).expect("file"));
    let c = lac_container::open(&mut r).expect("container");
    let (pcm, _) = lac_decode::decode_to_vec(c.as_ref(), &mut r).expect("pcm");
    let ch = usize::from(src.info().channels);
    for (k, p) in src.index().packets().iter().enumerate() {
        for (c_i, peak) in peaks.packet(k).expect("done").iter().enumerate() {
            let vals = p.sample_range().map(|i| pcm[i as usize * ch + c_i]);
            let q = |x: f32| (x * 32767.0).round() as i16;
            let (min, max) =
                vals.fold((i16::MAX, i16::MIN), |(a, b), x| (a.min(q(x)), b.max(q(x))));
            assert_eq!((peak.min, peak.max), (min, max), "packet {k} channel {c_i}");
        }
    }
}
